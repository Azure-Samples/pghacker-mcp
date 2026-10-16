// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Serves one commit to the code tools from a commit history of its own,
//! and writes its files out for ripgrep and GNU grep.
//!
//! The commit is copied into a bare repository of one commit whose tree has
//! exactly the source commit's blobs, under the path prefixes if any, on one
//! branch. Only git plumbing touches the blobs, so no checkout or attribute
//! (eol, text=auto, export-ignore, filters) can change a byte: the new
//! repository borrows the source's objects through `objects/info/alternates`,
//! takes the source commit's tree (or stages its entries under the prefixes
//! in an index of its own and writes that tree), and commits it with a fixed
//! author, committer, date and message, so the same source commit and
//! prefixes always give the same commit SHA. A repack then copies the
//! borrowed objects in, and the alternates go.
//!
//! The commit history is built from that repository into a schema of its
//! own, unless the schema already holds it, and the branch is resolved there
//! as the tools resolve a version.

use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow, bail, ensure};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use pghacker_code::history::{BuildOptions, build, store};
use pghacker_codesearch::Snapshot;
use pghacker_codesearch::snapshot::FileTable;
use tokio_postgres::Client;

/// Where the source commit comes from.
#[derive(Clone, Debug)]
pub enum Source {
    /// A clone that has the commit (bare or not, shallow or not).
    Local(PathBuf),
    /// A URL: only the commit is fetched, shallowly, into
    /// `<runtime>/git/<repo>-<sha>.git`.
    Remote(String),
}

/// A commit to serve.
#[derive(Clone, Debug)]
pub struct Spec {
    /// The repository's name, as the code tools name it.
    pub repo: String,
    pub source: Source,
    /// The source commit's full SHA.
    pub sha: String,
    /// Path prefixes to keep; empty keeps the whole tree.
    pub paths: Vec<String>,
    /// The branch the commit is on in the history.
    pub branch: String,
    /// The schema the history is built in.
    pub schema: String,
}

/// A served commit.
#[derive(Clone, Debug)]
pub struct Served {
    /// The commit as the code tools read it; its SHA is the one-commit
    /// repository's.
    pub snapshot: Snapshot,
    /// The one-commit repository.
    pub git_dir: PathBuf,
    /// The history was built now, not found built.
    pub built: bool,
    /// The files the tools have: symlinks and submodules are left out.
    pub files: u64,
    pub text_files: u64,
    pub binary_files: u64,
    /// The text files' lines.
    pub lines: u64,
}

/// The commit's author and committer, and when it was made.
const NAME: &str = "pghacker-replay";
const EMAIL: &str = "replay@pghacker.invalid";
const DATE: &str = "946684800 +0000";

/// The environment variables that would point git at other repositories,
/// indexes or objects than the arguments name.
const REDIRECTS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
];

/// git without the user's or the system's configuration (signing, commit
/// encodings, hooks paths) and without the variables that redirect it.
fn git() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    for var in REDIRECTS {
        cmd.env_remove(var);
    }
    cmd
}

/// git on the repository at `git_dir`.
fn git_at(git_dir: &Path) -> Command {
    let mut cmd = git();
    cmd.arg("--git-dir").arg(git_dir);
    cmd
}

/// git on the clone at `repo`: in its work tree if it has one, else on the
/// bare repository named explicitly, as safe.bareRepository may require.
fn git_in(repo: &Path) -> Command {
    if repo.join(".git").exists() {
        let mut cmd = git();
        cmd.arg("-C").arg(repo);
        cmd
    } else {
        git_at(repo)
    }
}

/// Runs a git command, feeding it `input`, and returns its output.
fn output(mut cmd: Command, input: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let what = format!("git {}", args.join(" "));
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("cannot run git")?;
    let writer = input.map(|data| {
        let mut stdin = child.stdin.take().expect("stdin is piped");
        std::thread::spawn(move || stdin.write_all(&data))
    });
    let out = child.wait_with_output()?;
    if let Some(writer) = writer {
        writer
            .join()
            .map_err(|_| anyhow!("the writer to git panicked"))?
            .with_context(|| format!("cannot write to {what}"))?;
    }
    if !out.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// A command's output as one trimmed line of text.
fn line(cmd: Command) -> Result<String> {
    Ok(String::from_utf8(output(cmd, None)?)?.trim().to_owned())
}

/// Whether `path` is one of the prefixes or below one.
fn in_paths(path: &[u8], prefixes: &[String]) -> bool {
    prefixes.is_empty()
        || prefixes.iter().any(|prefix| {
            let prefix = prefix.trim_end_matches('/').as_bytes();
            path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with(b"/"))
        })
}

/// Fetches only `sha` of `url`, shallowly, into `cache_dir/<repo>-<sha>.git`
/// unless it is there already (blocking). Returns the repository.
fn fetch(url: &str, cache_dir: &Path, repo: &str, sha: &str) -> Result<PathBuf> {
    let id =
        ObjectId::from_hex(sha.as_bytes()).with_context(|| format!("{sha} is not a full SHA"))?;
    let dir = cache_dir.join(format!("{repo}-{sha}.git"));
    if dir.exists() {
        if let Ok(fetched) = gix::open(&dir)
            && fetched.find_commit(id).is_ok()
        {
            return Ok(dir);
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("cannot remove the incomplete clone {}", dir.display()))?;
    }
    std::fs::create_dir_all(cache_dir)?;
    tracing::info!(%url, %sha, "fetching the commit");
    let interrupt = AtomicBool::new(false);
    let (fetched, _) = gix::prepare_clone_bare(url, &dir)
        .context("cannot prepare the clone")?
        .with_revision(Some(sha))
        .context("cannot pin the revision")?
        .with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(NonZeroU32::MIN))
        .fetch_only(gix::progress::Discard, &interrupt)
        .with_context(|| format!("cannot fetch {sha} from {url}"))?;
    fetched
        .find_commit(id)
        .with_context(|| format!("the fetch from {url} has no commit {sha}"))?;
    Ok(dir)
}

/// The source repository's object directory, absolute.
fn objects_dir(source: &Path) -> Result<PathBuf> {
    let mut cmd = git_in(source);
    cmd.args([
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "objects",
    ]);
    let dir = PathBuf::from(
        line(cmd).with_context(|| format!("{} is not a git repository", source.display()))?,
    );
    ensure!(dir.is_dir(), "{} has no objects", source.display());
    Ok(dir)
}

/// The one-commit commit's message: what it holds.
fn message(spec: &Spec) -> String {
    let mut message = format!("{} at {}", spec.repo, spec.sha);
    if !spec.paths.is_empty() {
        message.push_str(&format!("\n\nOnly {}.", spec.paths.join(", ")));
    }
    message
}

/// Builds the one-commit repository of `spec` at `dir`, replacing any
/// there, from the source's objects (blocking). Returns its commit.
fn build_repo(objects: &Path, spec: &Spec, dir: &Path) -> Result<String> {
    let tmp = dir.with_file_name(format!(
        "{}.tmp",
        dir.file_name()
            .context("no repository name")?
            .to_string_lossy()
    ));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    std::fs::create_dir_all(tmp.parent().context("no parent folder")?)?;
    let mut init = git();
    init.args(["init", "--bare", "-q"])
        .arg(format!("--initial-branch={}", spec.branch))
        .arg(&tmp);
    output(init, None)?;
    let alternates = tmp.join("objects/info/alternates");
    std::fs::write(&alternates, format!("{}\n", objects.display()))?;
    let mut has = git_at(&tmp);
    has.args(["cat-file", "-e", &format!("{}^{{commit}}", spec.sha)]);
    output(has, None).with_context(|| format!("the source has no commit {}", spec.sha))?;

    let tree = if spec.paths.is_empty() {
        let mut tree = git_at(&tmp);
        tree.args(["rev-parse", "--verify", &format!("{}^{{tree}}", spec.sha)]);
        line(tree)?
    } else {
        // ls-tree's records are the index-info format: mode, type, object,
        // a tab and the path, each ending in NUL.
        let mut list = git_at(&tmp);
        list.args(["ls-tree", "-r", "-z", &spec.sha]);
        let listing = output(list, None)?;
        let mut kept = Vec::new();
        for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .with_context(|| format!("unexpected ls-tree record {:?}", record.as_bstr()))?;
            if in_paths(&record[tab + 1..], &spec.paths) {
                kept.extend_from_slice(record);
                kept.push(0);
            }
        }
        if kept.is_empty() {
            bail!("{} has no files under {}", spec.sha, spec.paths.join(", "));
        }
        let index = tmp.join("replay.index");
        let mut stage = git_at(&tmp);
        stage
            .env("GIT_INDEX_FILE", &index)
            .args(["update-index", "-z", "--index-info"]);
        output(stage, Some(kept))?;
        let mut write = git_at(&tmp);
        write.env("GIT_INDEX_FILE", &index).arg("write-tree");
        let tree = line(write)?;
        std::fs::remove_file(&index)?;
        tree
    };

    let mut commit = git_at(&tmp);
    for (var, value) in [
        ("GIT_AUTHOR_NAME", NAME),
        ("GIT_AUTHOR_EMAIL", EMAIL),
        ("GIT_AUTHOR_DATE", DATE),
        ("GIT_COMMITTER_NAME", NAME),
        ("GIT_COMMITTER_EMAIL", EMAIL),
        ("GIT_COMMITTER_DATE", DATE),
    ] {
        commit.env(var, value);
    }
    // An encoding other than UTF-8 would add a header.
    commit
        .args([
            "-c",
            "i18n.commitEncoding=UTF-8",
            "commit-tree",
            "--no-gpg-sign",
        ])
        .args(["-m", &message(spec), &tree]);
    let sha = line(commit)?;
    let mut branch = git_at(&tmp);
    branch.args(["update-ref", &format!("refs/heads/{}", spec.branch), &sha]);
    output(branch, None)?;
    // Without -l, the pack takes the borrowed objects too.
    let mut repack = git_at(&tmp);
    repack.args(["repack", "-a", "-d", "-q"]);
    output(repack, None)?;
    std::fs::remove_file(&alternates)?;
    let mut complete = git_at(&tmp);
    complete.args(["rev-list", "--objects", "--all"]);
    output(complete, None).context("the one-commit repository is missing objects")?;

    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::rename(&tmp, dir)?;
    Ok(sha)
}

/// Whether `schema` holds the finished history of `commit` on `branch`,
/// keeping `paths`: pgh_reader is granted it last.
async fn has_history(
    admin: &Client,
    schema: &str,
    branch: &str,
    commit: &str,
    paths: &[String],
) -> Result<bool> {
    let table: Option<String> = admin
        .query_one(
            "SELECT to_regclass($1)::text",
            &[&format!("{schema}.code_history")],
        )
        .await?
        .get(0);
    if table.is_none() {
        return Ok(false);
    }
    let row = admin
        .query_opt(
            &format!(
                "SELECT head, master, paths, \
                        coalesce(has_table_privilege('pgh_reader', to_regclass($1), 'SELECT'), false) \
                 FROM {schema}.code_history"
            ),
            &[&format!("{schema}.code_line_instances")],
        )
        .await?;
    Ok(row.is_some_and(|row| {
        row.get::<_, Option<String>>(0).as_deref() == Some(commit)
            && row.get::<_, String>(1) == branch
            && row.get::<_, Vec<String>>(2) == paths
            && row.get::<_, bool>(3)
    }))
}

/// Serves `spec`'s commit: builds its one-commit repository under
/// `<runtime>/served`, builds the history into `spec.schema` as the
/// administrator at `admin_url` unless the schema has it already, and
/// resolves the branch with `reader`.
pub async fn serve(
    admin_url: &str,
    reader: &Client,
    runtime: &Path,
    spec: &Spec,
) -> Result<Served> {
    let schema = store::schema_name(&spec.schema)?.to_owned();
    let source = match &spec.source {
        Source::Local(path) => path.clone(),
        Source::Remote(url) => {
            let (url, cache, repo, sha) = (
                url.clone(),
                runtime.join("git"),
                spec.repo.clone(),
                spec.sha.clone(),
            );
            tokio::task::spawn_blocking(move || fetch(&url, &cache, &repo, &sha)).await??
        }
    };
    let git_dir = runtime.join("served").join(format!("{schema}.git"));
    let (s, dir) = (spec.clone(), git_dir.clone());
    let commit =
        tokio::task::spawn_blocking(move || build_repo(&objects_dir(&source)?, &s, &dir)).await??;

    let admin = pghacker_core::db::connect(admin_url, "pghacker-replay").await?;
    let built = !has_history(&admin, &schema, &spec.branch, &commit, &spec.paths).await?;
    if built {
        build(
            admin_url,
            &BuildOptions {
                git_dir: git_dir.clone(),
                master: spec.branch.clone(),
                url: None,
                schema: schema.clone(),
                paths: spec.paths.clone(),
                install: false,
            },
        )
        .await
        .with_context(|| format!("cannot build the history of {} in {schema}", spec.repo))?;
        ensure!(
            has_history(&admin, &schema, &spec.branch, &commit, &spec.paths).await?,
            "the history in {schema} is not of commit {commit}"
        );
    }

    let snapshot =
        pghacker_codesearch::rev::resolve(reader, &schema, &spec.repo, Some(&spec.branch)).await?;
    ensure!(
        snapshot.sha == commit,
        "{} resolves to {}, not {commit}",
        spec.branch,
        snapshot.sha
    );
    let table = FileTable::load(reader, &snapshot).await?;
    let text_files = table.files.iter().filter(|f| !f.is_binary).count() as u64;
    Ok(Served {
        files: table.files.len() as u64,
        text_files,
        binary_files: table.files.len() as u64 - text_files,
        lines: table.files.iter().map(|f| f.line_count as u64).sum(),
        snapshot,
        git_dir,
        built,
    })
}

/// Writes the text files of `commit` in the repository at `git_dir` under
/// `dir`, byte for byte as git has them: the tree that ripgrep and GNU grep
/// search for ground truth. Files the code tools have no lines for are left
/// out: binary ones (a NUL byte or invalid UTF-8), symlinks and submodules.
/// Files get the default permissions. Returns the number written. A
/// complete tree is reused: `<dir>.commit` records the commit it holds.
pub fn materialize(git_dir: &Path, commit: &str, dir: &Path) -> Result<u64> {
    let name = dir.file_name().context("no folder name")?.to_string_lossy();
    let marker = dir.with_file_name(format!("{name}.commit"));
    let repo = gix::open(git_dir).with_context(|| format!("cannot open {}", git_dir.display()))?;
    let id = ObjectId::from_hex(commit.as_bytes())?;
    let tree = repo.find_commit(id)?.tree()?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .context("cannot walk the commit's tree")?;
    let mut files = Vec::new();
    for entry in recorder.records {
        if !entry.mode.is_blob() {
            continue;
        }
        let path = entry
            .filepath
            .to_str()
            .map_err(|_| anyhow!("a path isn't UTF-8: {:?}", entry.filepath))?
            .to_owned();
        if path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        {
            bail!("unexpected path {path:?}");
        }
        let data = repo
            .find_blob(entry.oid)
            .with_context(|| format!("cannot read {path}"))?
            .take_data();
        if pghacker_code::split_lines(&data).is_some() {
            files.push((path, data));
        }
    }
    if dir.exists() && std::fs::read_to_string(&marker).is_ok_and(|sha| sha.trim() == commit) {
        return Ok(files.len() as u64);
    }
    if dir.exists() {
        if !marker.exists() && std::fs::read_dir(dir)?.next().is_some() {
            bail!(
                "{} exists and isn't a materialized tree; refusing to replace it",
                dir.display()
            );
        }
        std::fs::remove_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&marker);
    for (path, data) in &files {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, data)
            .with_context(|| format!("cannot write {}", target.display()))?;
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(&marker, commit)?;
    Ok(files.len() as u64)
}

/// A repository whose one commit tempts every filter, for the tests.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::{Path, PathBuf};

    use anyhow::Result;

    /// (path, mode, content): a CRLF and an LF file under
    /// `* text=auto eol=crlf`, an export-ignore file, one with a smudge
    /// filter, a binary file, a Latin-1 file, a symlink and a file outside
    /// `src`.
    pub const FILES: [(&str, &str, &[u8]); 9] = [
        (
            ".gitattributes",
            "100644",
            b"* text=auto eol=crlf\nsrc/kept.txt export-ignore\nsrc/quiet.txt filter=shout\n",
        ),
        ("src/crlf.c", "100644", b"int a;\r\nint b;\r\n"),
        ("src/lf.c", "100755", b"int c;\nint d;\n"),
        ("src/kept.txt", "100644", b"archives leave this out\n"),
        ("src/quiet.txt", "100644", b"quiet words\n"),
        ("src/data.bin", "100644", b"\x00\x01binary\n"),
        ("src/latin1.txt", "100644", b"caf\xe9\n"),
        ("src/link.c", "120000", b"lf.c"),
        ("docs/readme.md", "100644", b"outside src\r\n"),
    ];

    /// Text files to the code tools.
    pub const TEXT: [&str; 6] = [
        ".gitattributes",
        "src/crlf.c",
        "src/lf.c",
        "src/kept.txt",
        "src/quiet.txt",
        "docs/readme.md",
    ];

    /// Runs git in the clone at `dir` (a work tree, or else a bare
    /// repository) as a fixed author, with only the repository's
    /// configuration.
    pub fn git(dir: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut cmd = super::git_in(dir);
        cmd.args(args);
        for (var, value) in [
            ("GIT_AUTHOR_NAME", "Test"),
            ("GIT_AUTHOR_EMAIL", "test@example.org"),
            ("GIT_AUTHOR_DATE", "1767225600 +0000"),
            ("GIT_COMMITTER_NAME", "Test"),
            ("GIT_COMMITTER_EMAIL", "test@example.org"),
            ("GIT_COMMITTER_DATE", "1767225600 +0000"),
        ] {
            cmd.env(var, value);
        }
        super::output(cmd, input.map(<[u8]>::to_vec))
    }

    /// git's output as one trimmed line.
    pub fn text(dir: &Path, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8(git(dir, args, None)?)?.trim().to_owned())
    }

    /// A folder below the repository's ignored `.runtime`, removed when
    /// dropped.
    pub fn scratch() -> Result<tempfile::TempDir> {
        let root = pghacker_core::config::repo_root()
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let parent = root.join(".runtime/replay/tests");
        std::fs::create_dir_all(&parent)?;
        Ok(tempfile::Builder::new().prefix("t").tempdir_in(parent)?)
    }

    /// Commits [`FILES`] on `main` in a new work tree at `dir`, staging the
    /// blobs as written, and returns the commit. The smudge filter would
    /// upper-case `src/quiet.txt` on checkout.
    pub fn repo(dir: &Path) -> Result<String> {
        std::fs::create_dir_all(dir)?;
        let mut init = super::git();
        init.args(["init", "-q", "-b", "main"]).arg(dir);
        super::output(init, None)?;
        git(dir, &["config", "filter.shout.smudge", "tr a-z A-Z"], None)?;
        git(dir, &["config", "filter.shout.clean", "cat"], None)?;
        git(dir, &["config", "core.autocrlf", "true"], None)?;
        for (path, mode, content) in FILES {
            let blob = String::from_utf8(git(
                dir,
                &["hash-object", "-w", "--no-filters", "--stdin"],
                Some(content),
            )?)?;
            git(
                dir,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{},{path}", blob.trim()),
                ],
                None,
            )?;
        }
        let tree = text(dir, &["write-tree"])?;
        let commit = text(dir, &["commit-tree", "-m", "fixture", &tree])?;
        git(dir, &["update-ref", "refs/heads/main", &commit], None)?;
        Ok(commit)
    }

    /// A blob of `commit`, as git has it.
    pub fn blob(dir: &Path, commit: &str, path: &str) -> Result<Vec<u8>> {
        git(
            dir,
            &["cat-file", "blob", &format!("{commit}:{path}")],
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{self, FILES, TEXT};
    use super::*;
    use pghacker_codesearch::grep::{self, Limits};
    use pghacker_core::testing::TestDb;

    #[test]
    fn keeps_paths_under_the_prefixes() {
        let prefixes = vec![
            "src/backend/parser".to_owned(),
            "contrib/pg_trgm/".to_owned(),
        ];
        assert!(in_paths(b"src/backend/parser/gram.y", &prefixes));
        assert!(in_paths(b"contrib/pg_trgm/trgm.h", &prefixes));
        assert!(in_paths(b"src/backend/parser", &prefixes));
        assert!(!in_paths(b"src/backend/parser2/x.c", &prefixes));
        assert!(!in_paths(b"src/backend", &prefixes));
        assert!(in_paths(b"anything", &[]));
    }

    /// The PG19 replay's shape: a branch of the files under prefixes.
    #[tokio::test]
    async fn serves_the_files_under_the_prefixes() -> Result<()> {
        let scratch = fixture::scratch()?;
        let source = scratch.path().join("source");
        let sha = fixture::repo(&source)?;
        let db = TestDb::create().await?;
        let reader = db.reader().await?;
        let spec = Spec {
            repo: "postgres".into(),
            source: Source::Local(source.clone()),
            sha: sha.clone(),
            paths: vec!["src/".into()],
            branch: "REL_19_STABLE".into(),
            schema: "replay_test_prefixes".into(),
        };
        let served = serve(&db.admin_url, &reader, scratch.path(), &spec).await?;
        assert!(served.built);
        assert_eq!(served.snapshot.git_ref, "REL_19_STABLE");
        assert_eq!(served.snapshot.version.as_deref(), Some("19"));
        assert_eq!(
            served.git_dir,
            scratch.path().join("served/replay_test_prefixes.git")
        );
        assert!(!served.git_dir.join("objects/info/alternates").exists());

        // Exactly the source's entries under src/: modes, symlink and all.
        let listing = |dir: &Path, rev: &str| -> Result<Vec<String>> {
            let out = super::output(
                {
                    let mut cmd = git_at(dir);
                    cmd.args(["ls-tree", "-r", rev]);
                    cmd
                },
                None,
            )?;
            Ok(String::from_utf8(out)?.lines().map(str::to_owned).collect())
        };
        let ours = listing(&served.git_dir, "REL_19_STABLE")?;
        let theirs: Vec<String> = listing(&source.join(".git"), &sha)?
            .into_iter()
            .filter(|l| l.contains("\tsrc/"))
            .collect();
        assert_eq!(ours, theirs);
        assert_eq!(ours.len(), 7);

        let table = FileTable::load(&reader, &served.snapshot).await?;
        let mut paths: Vec<&str> = table.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "src/crlf.c",
                "src/data.bin",
                "src/kept.txt",
                "src/latin1.txt",
                "src/lf.c",
                "src/quiet.txt"
            ]
        );
        assert_eq!(
            (served.files, served.text_files, served.binary_files),
            (6, 4, 2)
        );
        assert_eq!(served.lines, 6);
        assert_eq!(table.loaded_paths, ["src/"]);

        // The same source commit and prefixes give the same commit.
        let again = serve(&db.admin_url, &reader, scratch.path(), &spec).await?;
        assert!(!again.built);
        assert_eq!(again.snapshot, served.snapshot);

        let tree = scratch.path().join("tree");
        let commit = &served.snapshot.sha;
        assert_eq!(materialize(&served.git_dir, commit, &tree)?, 4);
        for (path, _, content) in FILES {
            let written = std::fs::read(tree.join(path)).ok();
            let text = path.starts_with("src/") && TEXT.contains(&path);
            assert_eq!(written.as_deref(), text.then_some(content), "{path}");
        }
        assert_eq!(
            std::fs::read_to_string(tree.with_file_name("tree.commit"))?,
            *commit
        );
        // A complete tree is kept; a folder of other files is refused.
        assert_eq!(materialize(&served.git_dir, commit, &tree)?, 4);
        let other = scratch.path().join("other");
        std::fs::create_dir_all(&other)?;
        std::fs::write(other.join("mine"), "x")?;
        assert!(materialize(&served.git_dir, commit, &other).is_err());

        // Prefixes that hold nothing are refused.
        let mut empty = spec.clone();
        empty.paths = vec!["contrib".into()];
        empty.schema = "replay_test_empty".into();
        let error = serve(&db.admin_url, &reader, scratch.path(), &empty)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("has no files under contrib"),
            "{error:#}"
        );

        // grep reads the commit as the branch has it.
        let out = grep::run(
            &reader,
            &served.snapshot,
            "grep -rn 'int' src",
            Limits::default(),
        )
        .await?;
        assert_eq!(
            String::from_utf8(out.stdout)?,
            "src/crlf.c:1:int a;\r\nsrc/crlf.c:2:int b;\r\nsrc/lf.c:1:int c;\nsrc/lf.c:2:int d;\n"
        );
        Ok(())
    }
}
