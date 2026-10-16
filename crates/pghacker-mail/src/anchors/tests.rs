// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use chrono::TimeZone;

use super::*;

fn june_2026() -> Option<DateTime<Utc>> {
    Some(Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap())
}

fn values(anchors: &[AnchorRef], kind: &str) -> Vec<String> {
    anchors
        .iter()
        .filter(|a| a.kind == kind)
        .map(|a| a.value.clone())
        .collect()
}

fn body(text: &str) -> Vec<AnchorRef> {
    extract_at("", text, &[], june_2026())
}

#[test]
fn finds_tree_paths() {
    let text = "See src/backend/parser/gram.y. Also (contrib/pg_trgm/trgm_op.c:123), \
                doc/src/sgml/func.sgml, config/c-compiler.m4, contrib/pg_trgm and \
                https://github.com/postgres/postgres/blob/master/src/include/nodes/parsenodes.h#L10 \
                and https://git.postgresql.org/gitweb/?p=postgresql.git;a=blob;f=src/backend/utils/adt/json.c;hb=HEAD \
                and src/include/nodes/{a,b}.h";
    assert_eq!(
        values(&body(text), "file"),
        [
            "src/include/nodes/parsenodes.h",
            "src/backend/utils/adt/json.c",
            "src/backend/parser/gram.y",
            "contrib/pg_trgm/trgm_op.c",
            "doc/src/sgml/func.sgml",
            "config/c-compiler.m4",
            "contrib/pg_trgm",
            "src/include/nodes",
        ]
    );
}

#[test]
fn ignores_paths_that_are_not_the_tree() {
    let text = "https://github.com/someone/project/blob/main/src/backend/foo.c \
                /usr/local/src/backend/foo.c my_src/backend/x.c src/foo/bar.c \
                config/settings.json src/ doc/ src/backend/../x.c";
    assert!(values(&body(text), "file").is_empty(), "{:?}", body(text));
}

#[test]
fn finds_commits_in_clear_contexts() {
    let text = "Fixed by commit 1a2b3c4d. See commits abc1234, def5678 and 0a1b2c3d, \
                https://git.postgresql.org/gitweb/?p=postgresql.git;a=commit;h=0123abcd4567 \
                https://github.com/postgres/postgres/commit/89ABCDEF0123 postgr.es/c/a1b2c3d4e5 \
                and it was reverted in commit 9f8e7d6c5b4a.";
    assert_eq!(
        values(&body(text), "commit"),
        [
            "0123abcd4567",
            "89abcdef0123",
            "a1b2c3d4e5",
            "1a2b3c4d",
            "abc1234",
            "def5678",
            "0a1b2c3d",
            "9f8e7d6c5b4a"
        ]
    );
}

#[test]
fn ignores_hex_that_is_not_a_commit() {
    let text = "commit 1234567 is a number, commit deadbeef a word, abc1234 has no context, \
                index 1a2b3c4..5d6e7f8 100644, sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b \
                and the commitment 1a2b3c4d5e.";
    assert!(values(&body(text), "commit").is_empty(), "{:?}", body(text));
}

#[test]
fn finds_commitfest_entries() {
    let text = "https://commitfest.postgresql.org/58/6123/ and \
                https://commitfest.postgresql.org/patch/5988/ and CF entry #4567. \
                Not https://commitfest.postgresql.org/58/ or commitfest 58.";
    assert_eq!(values(&body(text), "commitfest"), ["6123", "5988", "4567"]);
}

#[test]
fn finds_named_releases() {
    let text = "Broken since PG 9.6, fine on PG19 and PostgreSQL 18; Postgres 17.2 too, \
                postgresql version 16, release 15, REL_14_STABLE, REL9_5_STABLE, PG20beta1.";
    assert_eq!(
        values(&body(text), "release"),
        ["9.6", "19", "18", "17", "16", "15", "20", "14", "9.5"]
    );
}

#[test]
fn finds_v_releases_in_release_contexts() {
    for (text, expected) in [
        ("Back-patched to v17.", "17"),
        ("This was introduced in v18, so", "18"),
        ("It works since v16~ and", "16"),
        ("Only v15+ has it.", "15"),
        ("Looking at the v14 branch.", "14"),
        ("This should go to v18 only.", "18"),
        ("Let's leave that for v20.", "20"),
        ("v13 and later behave the same.", "13"),
    ] {
        assert_eq!(values(&body(text), "release"), [expected], "{text}");
    }
    assert_eq!(
        values(&body("I was pondering about v14 and v15."), "release"),
        ["14", "15"]
    );
}

#[test]
fn ignores_patch_versions_and_other_numbers() {
    for text in [
        "Attached v17.",
        "Here's v17, rebased.",
        "Attached is the v17 patch.",
        "v17 of the patch fixes it.",
        "I looked at v13 and it is fine.",
        "In v10, I split the message.",
        "v12 looks good to me.",
        "Some review comments for v15.",
        "Thanks for v18!",
        "v3-0001-Fix.patch and v17-0002-Add.patch",
        "the v13 series from yesterday",
        "PostgreSQL 8.4 is old",
        "PG 99 and PG 1 and pg19",
        "version 17 of the doc",
        "9.6 alone",
        "release 1.2",
        "Fixed in v25.",
    ] {
        assert!(
            values(&body(text), "release").is_empty(),
            "{text}: {:?}",
            body(text)
        );
    }
    let subject = extract_at("Re: [PATCH v17 1/3] Fix", "", &[], june_2026());
    assert!(values(&subject, "release").is_empty());
}

#[test]
fn a_message_s_own_patch_versions_are_not_releases() {
    let patch = PatchFile {
        filename: "v18-0001-Fix.patch".into(),
        series_version: Some(18),
        paths: vec!["src/backend/a.c".into(), "lib/other.c".into()],
        ..Default::default()
    };
    let anchors = extract_at("Fix", "Changes in v18 are small.", &[patch], june_2026());
    assert!(values(&anchors, "release").is_empty());
    assert_eq!(values(&anchors, "file"), ["src/backend/a.c"]);
    assert_eq!(anchors[0].origin, "patch");
}

#[test]
fn releases_are_bounded_by_the_date() {
    let in_2015 = Some(Utc.with_ymd_and_hms(2015, 6, 1, 0, 0, 0).unwrap());
    let anchors = extract_at("", "Back-patched to v15 and PG 9.4.", &[], in_2015);
    assert_eq!(values(&anchors, "release"), ["9.4"]);
}

#[test]
fn origins_prefer_subject_then_body_then_patch() {
    let patch = PatchFile {
        paths: vec!["src/backend/a.c".into(), "src/backend/b.c".into()],
        ..Default::default()
    };
    let anchors = extract_at(
        "[PATCH] Fix src/backend/a.c on PG18",
        "In src/backend/a.c and src/backend/b.c, for PG18.",
        &[patch],
        june_2026(),
    );
    let summary: Vec<_> = anchors
        .iter()
        .map(|a| (a.kind, a.value.as_str(), a.origin))
        .collect();
    assert_eq!(
        summary,
        [
            ("file", "src/backend/a.c", "subject"),
            ("release", "18", "subject"),
            ("file", "src/backend/b.c", "body"),
        ]
    );
}
