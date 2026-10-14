use super::*;
use crate::parse::parse;

const MULTIPART: &[u8] = include_bytes!("../../tests/fixtures/multipart_patch.eml");
const COMPRESSED: &[u8] = include_bytes!("../../tests/fixtures/compressed_patches.eml");

#[test]
fn series_come_from_file_names() {
    assert_eq!(series("v3-0001-Fix-foo.patch"), (Some(3), Some(1)));
    assert_eq!(series("0002-Fix-foo.patch"), (None, Some(2)));
    assert_eq!(series("postgres_fdw-fix-v2.patch"), (Some(2), None));
    assert_eq!(
        series("v20260601-0007-View.patch"),
        (Some(20260601), Some(7))
    );
    assert_eq!(series("v6a-0001-Add-CI.patch"), (Some(6), Some(1)));
    assert_eq!(series("nocfbot-v47-0001-refactor.txt"), (Some(47), Some(1)));
    assert_eq!(series("vXXX1-0001-Draft.cfbotignorepatch"), (None, Some(1)));
    assert_eq!(series("fix.patch"), (None, None));
    assert_eq!(series("report-2026-06-01.diff"), (None, None));
    assert_eq!(
        subject_series("Re: [PATCH v3 2/7] Fix foo"),
        (Some(3), Some(2))
    );
    assert_eq!(subject_series("[PATCH] Fix foo"), (None, None));
    assert_eq!(subject_series("Fix foo v3"), (None, None));
}

#[test]
fn counts_unified_diffs_by_hunk() {
    let diff = "\
diff --git a/src/backend/a.c b/src/backend/a.c
index 1111111..2222222 100644
--- a/src/backend/a.c
+++ b/src/backend/a.c
@@ -1,4 +1,5 @@
 int i;
-++i;
+++i;
+--i;
 
 return;
diff --git a/src/include/new.h b/src/include/new.h
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/include/new.h
@@ -0,0 +1,2 @@
+#define A
+--- not a header
diff --git a/doc/old.txt b/doc/old.txt
deleted file mode 100644
--- a/doc/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-gone
diff --git a/src/tools/x.pl b/src/tools/y.pl
similarity index 90%
rename from src/tools/x.pl
rename to src/tools/y.pl
Binary files a/src/test/a.bin and b/src/test/a.bin differ
";
    let stats = diff_stats(diff);
    assert_eq!(
        stats.paths,
        [
            "src/backend/a.c",
            "src/include/new.h",
            "doc/old.txt",
            "src/tools/y.pl",
            "src/test/a.bin"
        ]
    );
    assert_eq!((stats.added, stats.removed, stats.hunks), (4, 2, 3));
}

#[test]
fn counts_context_and_plain_diffs() {
    let context = "\
*** src/backend/a.c.orig\t2026-06-01 10:00:00.000000000 +0200
--- src/backend/a.c\t2026-06-01 10:05:00.000000000 +0200
***************
*** 1,3 ****
  int i;
! int j;
- int k;
--- 1,3 ----
  int i;
! long j;
+ int l;
";
    let stats = diff_stats(context);
    assert_eq!(stats.paths, ["src/backend/a.c"]);
    assert_eq!((stats.added, stats.removed), (2, 2));
    let plain = "\
--- postgresql.orig/contrib/pg_trgm/trgm_op.c\t2026-06-01 10:00:00
+++ postgresql/contrib/pg_trgm/trgm_op.c\t2026-06-01 10:05:00
@@ -1 +1 @@
-a
+b
";
    let stats = diff_stats(plain);
    assert_eq!(stats.paths, ["contrib/pg_trgm/trgm_op.c"]);
    assert_eq!((stats.added, stats.removed), (1, 1));
}

#[test]
fn normalizes_paths() {
    assert_eq!(
        normalize_path("b/src/backend/a.c").as_deref(),
        Some("src/backend/a.c")
    );
    assert_eq!(normalize_path("./src/a.c").as_deref(), Some("src/a.c"));
    assert_eq!(
        normalize_path("/home/me/pg/src/backend/a.c").as_deref(),
        Some("src/backend/a.c")
    );
    assert_eq!(
        normalize_path("meson.build").as_deref(),
        Some("meson.build")
    );
    assert_eq!(
        normalize_path("lib/other.c").as_deref(),
        Some("lib/other.c")
    );
    assert_eq!(normalize_path("/dev/null"), None);
    assert!(is_tree_path("src/backend/a.c"));
    assert!(is_tree_path("meson.build"));
    assert!(!is_tree_path("lib/other.c"));
    assert!(!is_tree_path("src"));
}

#[test]
fn extracts_attached_patches() {
    let msg = parse(MULTIPART).unwrap();
    let patches = extract(&msg);
    assert_eq!(patches.len(), 2, "{patches:?}");
    let first = &patches[0];
    assert_eq!(first.filename, "v3-0001-Fix-the-frobnicator.patch");
    assert_eq!((first.series_version, first.part), (Some(3), Some(1)));
    assert_eq!(
        first.paths,
        ["src/backend/utils/adt/frob.c", "src/include/utils/frob.h"]
    );
    assert_eq!((first.added, first.removed), (3, 1));
    assert!(first.content.starts_with("From 89abcdef"));
    let second = &patches[1];
    assert_eq!((second.series_version, second.part), (Some(3), Some(2)));
    assert_eq!(second.paths, ["src/test/regress/sql/frob.sql"]);
    assert_eq!((second.added, second.removed), (2, 0));
}

#[test]
fn extracts_compressed_and_misnamed_patches() {
    let msg = parse(COMPRESSED).unwrap();
    let patches = extract(&msg);
    let names: Vec<_> = patches.iter().map(|p| p.filename.as_str()).collect();
    assert_eq!(
        names,
        [
            "v4-0001-Rename.patch",
            "v5-0001-First.patch",
            "v5-0002-Second.patch",
            "v1-0001-encoded-name.patch"
        ]
    );
    for patch in &patches {
        assert_eq!(patch.paths, ["contrib/pg_trgm/trgm_op.c"]);
        assert_eq!((patch.added, patch.removed), (1, 1));
    }
    assert_eq!(
        (patches[1].series_version, patches[1].part),
        (Some(5), Some(1))
    );
}

fn message(subject: &str, body: &str, attachments: Vec<Attachment>) -> ParsedMessage {
    ParsedMessage {
        message_id: "m@example.org".into(),
        subject: subject.into(),
        body: body.into(),
        attachments,
        ..Default::default()
    }
}

fn text_attachment(name: &str, content_type: &str, text: &str) -> Attachment {
    Attachment {
        filename: Some(name.into()),
        content_type: content_type.into(),
        data: text.as_bytes().to_vec(),
        text: Some(text.into()),
    }
}

const SMALL_DIFF: &str =
    "diff --git a/src/a.c b/src/a.c\n--- a/src/a.c\n+++ b/src/a.c\n@@ -1 +1 @@\n-a\n+b\n";

#[test]
fn diffs_under_other_names_count_only_by_content() {
    let msg = message(
        "x",
        "",
        vec![
            text_attachment("nocfbot-0001-x.txt", "text/plain", SMALL_DIFF),
            text_attachment("notes.txt", "text/plain", "no diff"),
            text_attachment("repro.sql", "application/sql", SMALL_DIFF),
            text_attachment("regression.diffs", "text/plain", SMALL_DIFF),
            text_attachment("odd-name", "text/x-patch", "whatever"),
            Attachment {
                filename: Some("image.patch".into()),
                content_type: "application/octet-stream".into(),
                data: vec![0x89, b'P', b'N', b'G', 0, 0],
                text: None,
            },
        ],
    );
    let names: Vec<_> = extract(&msg).into_iter().map(|p| p.filename).collect();
    assert_eq!(names, ["nocfbot-0001-x.txt", "odd-name"]);
}

#[test]
fn extracts_inline_diffs_outside_quotes() {
    let body = format!(
        "Something like this?\n\n{SMALL_DIFF}\nAnd a quoted one:\n> diff --git a/src/b.c b/src/b.c\n> --- a/src/b.c\n> +++ b/src/b.c\n> @@ -1 +1 @@\n> -a\n> +b\n\nAnd again:\n\n--- src/c.c\n+++ src/c.c\n@@ -1,2 +1,2 @@\n x\n-y\n+z\n"
    );
    let msg = message("Re: [PATCH v7 3/4] Do things", &body, Vec::new());
    let patches = extract(&msg);
    assert_eq!(patches.len(), 2);
    assert_eq!(patches[0].filename, "inline-1");
    assert_eq!(patches[0].paths, ["src/a.c"]);
    assert_eq!(
        (patches[0].series_version, patches[0].part),
        (Some(7), Some(3))
    );
    assert_eq!(patches[0].content, SMALL_DIFF);
    assert_eq!(patches[1].filename, "inline-2");
    assert_eq!(patches[1].paths, ["src/c.c"]);
    assert_eq!((patches[1].added, patches[1].removed), (1, 1));
}

#[test]
fn snippets_without_headers_are_not_patches() {
    let msg = message("x", "Maybe:\n+ a\n- b\n@@ hmm\n", Vec::new());
    assert!(extract(&msg).is_empty());
}

#[test]
fn duplicate_names_stay_unique() {
    let msg = message(
        "x",
        "",
        vec![
            text_attachment("fix.patch", "text/x-patch", SMALL_DIFF),
            text_attachment("fix.patch", "text/x-patch", SMALL_DIFF),
        ],
    );
    let names: Vec<_> = extract(&msg).into_iter().map(|p| p.filename).collect();
    assert_eq!(names, ["fix.patch", "fix.patch (2)"]);
}

#[test]
fn diff_lines_are_classified() {
    use super::LineKind::{Added, Context, Other, Removed};
    let unified = "Subject: fix\n\
        ---\n\
        diff --git a/src/a.sql b/src/a.sql\n\
        --- a/src/a.sql\n\
        +++ b/src/a.sql\n\
        @@ -1,3 +1,3 @@\n \
        keep\n\
        --- a comment\n\
        +-- the comment\n \
        end\n\
        diff --git a/src/b.c b/src/b.c\n\
        --- a/src/b.c\n\
        +++ b/src/b.c\n\
        @@ -1 +1,2 @@\n\
        -x\n\
        +y\n\
        +\n";
    let (stats, lines) = super::parse_diff(unified);
    assert_eq!(stats.paths, ["src/a.sql", "src/b.c"]);
    let kinds: Vec<(super::LineKind, &str, Option<usize>)> =
        lines.iter().map(|l| (l.kind, l.text, l.file)).collect();
    assert_eq!(
        kinds,
        [
            (Other, "Subject: fix", None),
            (Other, "---", None),
            (Other, "diff --git a/src/a.sql b/src/a.sql", Some(0)),
            (Other, "--- a/src/a.sql", Some(0)),
            (Other, "+++ b/src/a.sql", Some(0)),
            (Other, "@@ -1,3 +1,3 @@", Some(0)),
            (Context, "keep", Some(0)),
            (Removed, "-- a comment", Some(0)),
            (Added, "-- the comment", Some(0)),
            (Context, "end", Some(0)),
            (Other, "diff --git a/src/b.c b/src/b.c", Some(1)),
            (Other, "--- a/src/b.c", Some(1)),
            (Other, "+++ b/src/b.c", Some(1)),
            (Other, "@@ -1 +1,2 @@", Some(1)),
            (Removed, "x", Some(1)),
            (Added, "y", Some(1)),
            (Added, "", Some(1)),
        ]
    );
    assert_eq!((stats.added, stats.removed), (3, 2));

    let context = "*** a/src/c.c\t2009-01-01\n\
        --- b/src/c.c\t2009-01-02\n\
        ***************\n\
        *** 1,2 ****\n\
        ! old\n  \
        same\n\
        --- 1,3 ----\n\
        ! new\n\
        + more\n  \
        same\n";
    let (stats, lines) = super::parse_diff(context);
    assert_eq!(stats.paths, ["src/c.c"]);
    let kinds: Vec<(super::LineKind, &str)> = lines
        .iter()
        .filter(|l| l.kind != Other)
        .map(|l| (l.kind, l.text))
        .collect();
    assert_eq!(
        kinds,
        [
            (Removed, "old"),
            (Context, "same"),
            (Added, "new"),
            (Added, "more"),
            (Context, "same")
        ]
    );
}
