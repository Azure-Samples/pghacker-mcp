// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use chrono::TimeZone;

use super::*;

fn on(y: i32, m: u32, d: u32) -> Option<DateTime<Utc>> {
    Some(Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap())
}

/// A name, when it was posted, and the version and part it gives.
type Case<'a> = (&'a str, Option<DateTime<Utc>>, Option<&'a str>, Option<i32>);

fn check(cases: &[Case<'_>]) {
    for &(name, sent, version, part) in cases {
        let series = name_series(name, sent);
        assert_eq!(
            (series.version.as_deref(), series.part),
            (version, part),
            "{name}"
        );
    }
}

#[test]
fn labels_before_a_part() {
    let any = on(2025, 6, 1);
    check(&[
        (
            "v49-0001-Add-JSON_TABLE-function.patch",
            any,
            Some("49"),
            Some(1),
        ),
        ("nocfbot-v47-0001-refactor.txt", any, Some("47"), Some(1)),
        (
            "rel_17_v8-0001-Restrict-copying-of-invalidated-replication-slots.patch",
            any,
            Some("8"),
            Some(1),
        ),
        (
            "pg10-11-12-v5-0001-Fix-the-memory-leak-in-psql-describe.patch",
            any,
            Some("5"),
            Some(1),
        ),
        (
            "v9_0001_use_index_on_subs_when_pub_rep_ident_full.patch",
            any,
            Some("9"),
            Some(1),
        ),
        (
            "v0-001-fix-alias-values-rte+cte.patch",
            any,
            Some("0"),
            Some(1),
        ),
        (
            "v1-00001-copy-json-exclude-generated-columns.patch",
            any,
            Some("1"),
            Some(1),
        ),
        (
            "v8.1-0006-Handle-pg_get_triggerdef-default-args-in-system.patch",
            any,
            Some("8.1"),
            Some(6),
        ),
        (
            "v10a-0001-ci-add-github-actions-based-ci.patch",
            any,
            Some("10a"),
            Some(1),
        ),
        (
            "v01-0007-Decouple-updating-of-freezing-information-from-swap_.patch",
            any,
            Some("01"),
            Some(7),
        ),
        (
            "v2899-0002-Small-improvements-for-radixtree-and-tests.patch",
            any,
            Some("2899"),
            Some(2),
        ),
        (
            "v20261015-0004-Add-zstd-compression-dictionaries-with-a-c.patch",
            any,
            Some("20261015"),
            Some(4),
        ),
        (
            "v06082026-0001-pg_basebackup-rename-the-blackhole-backup-.patch",
            any,
            Some("06082026"),
            Some(1),
        ),
        (
            "v2025-06-30-0001-Add-gist_index_check-function-to-verify-.patch",
            any,
            Some("2025-06-30"),
            Some(1),
        ),
        (
            "v20220130-2-0001-session-variables.patch",
            any,
            Some("20220130-2"),
            Some(1),
        ),
        (
            "v95_2-0002-Document-the-steps-to-check-if-the-standby-is-r.patch",
            any,
            Some("95_2"),
            Some(2),
        ),
        (
            "v12-pg13-0001-Fix-memory-leak-about-attrmap.patch",
            any,
            Some("12-pg13"),
            Some(1),
        ),
        (
            "v28-review-0006-pgindent.patch",
            any,
            Some("28-review"),
            Some(6),
        ),
        (
            "v6.1-REL_15_STABLE-0001-Fix-deadlock-detector-activation-.patch",
            any,
            Some("6.1-REL_15_STABLE"),
            Some(1),
        ),
        (
            "vpg18-0001-fixup-5f003855e7-for-srclen-0.patch",
            any,
            Some("pg18"),
            Some(1),
        ),
        (
            "vfinal-0001-Update-Unicode-data-to-Unicode-16.0.0.patch",
            any,
            Some("final"),
            Some(1),
        ),
        (
            "vxxx1-0001-Add-parttioned-clocksweep-and-numa-goodies.cfbotignorepatch",
            any,
            Some("xxx1"),
            Some(1),
        ),
        (
            "v-13-0001-New-options-engine.patch",
            any,
            Some("13"),
            Some(1),
        ),
        (
            "recovery-in-pgcontrol-v7-0002-remove-backuplabel.patch",
            any,
            Some("7"),
            Some(2),
        ),
        (
            "brin-inclusion-v07-01-sql-level-support-functions.patch",
            any,
            Some("07"),
            Some(1),
        ),
        ("0001-glossary-v4-01.patch", any, Some("4"), Some(1)),
        (
            "0001-v35-0001-Add-pg_get_process_memory_context-function.patch",
            any,
            Some("35"),
            Some(1),
        ),
        ("v1-0001-Fix-race.patch (2)", any, Some("1"), Some(1)),
        // A label doesn't run into the slug, nor start a word.
        ("v4-fix-the-bad-thing-0015-foo.patch", any, Some("4"), None),
        ("help-variables-0001-foo.patch", any, None, Some(1)),
        // Up to 24 characters, a label is the version; a longer one is a
        // slug the part ends, and its first token the version.
        (
            "v20241231-single-spill-0001-Hash-join-with-a-single-spill.patch",
            on(2024, 12, 31),
            Some("20241231-single-spill"),
            Some(1),
        ),
        (
            "v2-Opclass-bitwise-equality-0002.patch",
            on(2019, 10, 28),
            Some("2"),
            Some(2),
        ),
        (
            "v8_indexlist_contains_partitioned_indexes-0001-v8.patch",
            on(2022, 10, 1),
            Some("8"),
            Some(1),
        ),
    ]);
}

#[test]
fn labels_after_a_part() {
    let any = on(2025, 6, 1);
    check(&[
        (
            "0002-v4-Feat-Add-support-for-the-SIMD-AVX-512-crc32c-algorit.patch",
            any,
            Some("4"),
            Some(2),
        ),
        (
            "0001-v0002-Surface-tab-completions-for-debugging.patch",
            any,
            Some("0002"),
            Some(1),
        ),
        // "v15 docs": a release as often as a version.
        (
            "0029-v15-doc-review-Some-additional-information-about-whe.patch",
            on(2022, 6, 1),
            None,
            Some(29),
        ),
    ]);
}

#[test]
fn versions_first_last_and_inside() {
    let any = on(2025, 6, 1);
    check(&[
        ("v0-backend-size.patch", any, Some("0"), None),
        ("v9_tid_range_scans.patch", any, Some("9"), None),
        (
            "v001_avoid_race_condition_for_event_trigger_test.patch",
            any,
            Some("001"),
            None,
        ),
        (
            "v4.Make-fetch_size-settable-per-server-and-table.patch",
            any,
            Some("4"),
            None,
        ),
        ("absorb-openbsd-fixes-v3.patch", any, Some("3"), None),
        (
            "zerofill_walblock_on_standby_v2.patch",
            any,
            Some("2"),
            None,
        ),
        ("acl-missing-prototypes.v1.patch", any, Some("1"), None),
        ("0000-jsonpath-v21.patch", any, Some("21"), Some(0)),
        (
            "0017-Extend-handling-of-concurrent-aborts-for-streamin-v3.patch",
            any,
            Some("3"),
            Some(17),
        ),
        ("pageindextupleoverwrite v2.patch", any, Some("2"), None),
        (
            "wfunc_pushdown_partitionby_v0.4.patch",
            any,
            Some("0.4"),
            None,
        ),
        ("atomic_subxids.v3a.patch", any, Some("3a"), None),
        ("temporal_pks_v0004.patch", any, Some("0004"), None),
        ("walwriter_latch_v1_2012_05_02.patch", any, Some("1"), None),
        // A decimal has up to three digits: a date after the version isn't one.
        (
            "insert_on_dup.v3.2013_09_10.patch",
            on(2013, 9, 11),
            Some("3"),
            None,
        ),
        (
            "pg_stat_statements_ext_text.v5.2013_12_09.patch",
            on(2013, 12, 10),
            Some("5"),
            None,
        ),
        (
            "insert_on_dup.v3.2013_09_10-review-notes.patch",
            on(2013, 9, 11),
            Some("3"),
            None,
        ),
        ("amcheck-gist-v6-heikki.patch", any, Some("6"), None),
        ("fix_pg_dump_ordering_v2-96.patch", any, Some("2"), None),
        (
            "0001-Add-optional-second-hash-proc-v2-wip.patch",
            any,
            Some("2"),
            Some(1),
        ),
        (
            "0001-Add-kqueue-2-support-for-waitevent-v13-REL_11_STABLE.patch",
            on(2018, 6, 1),
            Some("13"),
            Some(1),
        ),
        ("0002-gtt-v52-regress.patch", any, Some("52"), Some(2)),
        ("at2v5-domains.patch", any, Some("5"), None),
        ("VS2010v10.patch", any, Some("10"), None),
        ("hash_partition.patch.v2", any, Some("2"), None),
        (
            "patch_pg_current_logfile-v14.diff.abstract_guc_part1",
            any,
            Some("14"),
            None,
        ),
        (
            "pgsql-v9.2-fix-leaky-view.part-1.v7.patch",
            on(2012, 1, 10),
            Some("7"),
            None,
        ),
        (
            "brin-inclusion-v05-remove-assert-checking.patch",
            any,
            Some("05"),
            None,
        ),
        (
            "multi_sync_replication_v23_list_member_int.diff",
            any,
            Some("23"),
            None,
        ),
        (
            "0001-PATCH-v3-1-3-Add-new-boolean-reloption-security_invo.patch",
            any,
            Some("3"),
            Some(1),
        ),
        // 7.1 is long past in 2020: a version.
        (
            "v7.1-Add-GIST-sortsupport-btree-gist.patch",
            on(2020, 6, 1),
            Some("7.1"),
            None,
        ),
        ("9.1-planner-speedup-v6.patch", any, Some("6"), None),
    ]);
}

#[test]
fn releases_references_and_subject_text_are_not_versions() {
    check(&[
        (
            "v7.1-Add-GIST-sortsupport-btree-gist.patch",
            on(2001, 3, 1),
            None,
            None,
        ),
        ("syncrep-v9.3.patch", on(2013, 3, 1), None, None),
        ("pgsql-v12.8-auth-delay.1.patch", on(2021, 9, 1), None, None),
        (
            "fix-memory-leak-in-toast-hash_v13-to-v9.6.patch",
            on(2021, 3, 1),
            None,
            None,
        ),
        (
            "0001-Bloom-patch-for-v10-and-v9.6.patch",
            on(2017, 9, 1),
            None,
            Some(1),
        ),
        (
            "0003-doc-typo-since-v10.patch",
            on(2018, 1, 1),
            None,
            Some(3),
        ),
        (
            "nocfbot.v16-v18.pg_test_timing-backpatch.diff",
            on(2025, 6, 1),
            None,
            None,
        ),
        (
            "0001-Add-PGDLLEXPORT-to-PG_FUNCTION_INFO_V1-function-decl.patch",
            None,
            None,
            Some(1),
        ),
        (
            "0001-Move-contrib-seg-to-only-use-V1-calling-conventions.patch",
            None,
            None,
            Some(1),
        ),
        (
            "0004-WIP-v16-docs-Add-more-details-about-pg_stat_get_xact.patch",
            on(2023, 1, 1),
            None,
            Some(4),
        ),
        (
            "0001-Fix-constant-when-extracting-timestamp-from-uuidv7.patch",
            None,
            None,
            Some(1),
        ),
        (
            "0001-Add-logical_decoding_work_mem-to-limit-ReorderBuffer.nov16.patch",
            None,
            None,
            Some(1),
        ),
        ("writeablectev1.patch", None, None, None),
        ("xlogreader_9.4.1.diff", None, None, None),
        (
            "0001-fix-ssl-test-on-openssl-4.0.patch",
            None,
            None,
            Some(1),
        ),
        ("patch-plperl-7.0.2.patch", None, None, None),
        (
            "pgsql-v9.2-drop-reworks-part-0.2.patch",
            on(2012, 1, 1),
            None,
            None,
        ),
        // A word such as "for" vetoes only a release of the time.
        (
            "20240612_PS_nitpicks_for_v4.txt",
            on(2024, 6, 12),
            Some("4"),
            None,
        ),
        ("wait_for_v2.patch", on(2020, 1, 1), Some("2"), None),
    ]);
}

#[test]
fn dates_and_zero_decimals() {
    check(&[
        (
            "0018-Re-implement-UI-and-synchronization-for-res-20251013.patch",
            on(2025, 10, 13),
            Some("20251013"),
            Some(18),
        ),
        (
            "xmltable-20160909.patch",
            on(2016, 9, 9),
            Some("20160909"),
            None,
        ),
        (
            "0001-2021-11-17.patch",
            on(2021, 11, 17),
            Some("2021-11-17"),
            Some(1),
        ),
        (
            "poorman-hll-win32.2014_07_26.patch",
            on(2014, 7, 26),
            Some("2014_07_26"),
            None,
        ),
        (
            "20110218_initialize_processed.patch",
            on(2011, 2, 18),
            Some("20110218"),
            None,
        ),
        (
            "20020822_02_pitr.patch",
            on(2002, 8, 22),
            Some("20020822"),
            Some(2),
        ),
        (
            "add-support-for-time-based-lag-tracking-to-logical-170429.patch",
            on(2017, 4, 30),
            Some("170429"),
            None,
        ),
        (
            "add-support-for-time-based-lag-tracking-to-logical-170429.patch",
            on(2018, 1, 1),
            None,
            None,
        ),
        ("foo-20231345.patch", on(2023, 1, 1), None, None),
        ("foo-20300101.patch", on(2023, 1, 1), None, None),
        ("trgm_wildcard-0.4.patch", None, Some("0.4"), None),
        ("gist_fast_build-0.5.0.patch", None, Some("0.5.0"), None),
    ]);
}

#[test]
fn parts_come_only_from_reliable_places() {
    check(&[
        ("0000-cover-letter.patch", None, None, Some(0)),
        ("07_readme.patch", None, None, Some(7)),
        ("003_session_hooks_samples.patch", None, None, Some(3)),
        ("00001_free_function_memory.patch", None, None, Some(1)),
        ("nocfbot-0001-x.txt", None, None, Some(1)),
        (
            "REL_16_0002-Alter-slot-option-two_phase-only-when-altering-true-.patch",
            None,
            None,
            Some(2),
        ),
        (
            "9.2-0001-Use-critical-section-when-ensuring-empty-pages-are-i.patch",
            None,
            None,
            Some(1),
        ),
        (
            "1-0001-push-join-with-function-scan-to-remote-server.patch",
            None,
            None,
            Some(1),
        ),
        ("19736-null-key-validation.patch", None, None, None),
        ("080_pg_isready.pl.patch", None, None, None),
        ("adhoc-035-failure-debugging.patch", None, None, None),
        ("fix.patch (2)", None, None, None),
        ("inline-1", None, None, None),
        ("inline-2 (2)", None, None, None),
    ]);
}

fn thread<'a>(posts: &[(i64, Option<DateTime<Utc>>, &'a str, &'a str)]) -> Vec<Option<String>> {
    let patches: Vec<ThreadPatch<'a>> = posts
        .iter()
        .map(|&(message, sent_at, subject, filename)| ThreadPatch {
            message,
            sent_at,
            subject,
            filename,
        })
        .collect();
    thread_series(&patches)
        .into_iter()
        .map(|s| s.version)
        .collect()
}

#[test]
fn bare_numbers_count_when_a_thread_s_posts_rise() {
    let s = |v: &str| Some(v.to_owned());
    // In any order.
    assert_eq!(
        thread(&[
            (3, on(2020, 3, 1), "Re: x", "psql-dp-5.patch"),
            (1, on(2020, 1, 1), "x", "psql-dp-3.patch"),
            (2, on(2020, 2, 1), "Re: x", "psql-dp-4.patch"),
        ]),
        [s("5"), s("3"), s("4")]
    );
    assert_eq!(
        thread(&[
            (1, on(2014, 1, 1), "x", "array_offset-04.patch"),
            (2, on(2014, 2, 1), "Re: x", "array_offset-06.patch"),
            (3, on(2014, 3, 1), "Re: x", "indexonlyscan_gist_2.0.patch"),
            (4, on(2014, 4, 1), "Re: x", "indexonlyscan_gist_3.0.patch"),
        ]),
        [s("04"), s("06"), s("2.0"), s("3.0")]
    );
    // Alone, falling, or several in one post: not versions.
    assert_eq!(
        thread(&[(1, on(2014, 1, 1), "x", "phantomcid-4.patch")]),
        [None]
    );
    assert_eq!(
        thread(&[
            (1, on(2014, 1, 1), "x", "foo-3.patch"),
            (2, on(2014, 2, 1), "Re: x", "foo-2.patch"),
        ]),
        [None, None]
    );
    assert_eq!(
        thread(&[
            (1, on(2014, 1, 1), "x", "dblink_check-93.patch"),
            (1, on(2014, 1, 1), "x", "dblink_check-94.patch"),
            (2, on(2014, 2, 1), "Re: x", "dblink_check-95.patch"),
        ]),
        [None, None, None]
    );
}

#[test]
fn subjects_give_versions_the_names_lack() {
    assert_eq!(
        subject_version("[PATCH v3 2/7] Fix foo").as_deref(),
        Some("3")
    );
    assert_eq!(
        subject_version("[RFC PATCH v2 RESEND 01/10] umbra").as_deref(),
        Some("2")
    );
    assert_eq!(subject_version("[PATCH 1/2 v3] Fix").as_deref(), Some("3"));
    assert_eq!(subject_version("[PATCH v.7a] Fix").as_deref(), Some("7a"));
    assert_eq!(subject_version("Re: [PATCH v3 2/7] Fix foo"), None);
    assert_eq!(subject_version("AW: [PATCH v2] Fix"), None);
    assert_eq!(subject_version("[PATCH] Fix foo"), None);
    assert_eq!(subject_version("Fix foo v3"), None);
    for versioned in [
        "JIT compiling with LLVM v11",
        "WIP: Faster Expression Processing v4",
        "[PATCH v1] Allow COPY \"text\" to output a header",
        "better atomics - v0.5",
        "Refactor the [v2] parser",
        "[PATCH v.7a] Fix",
    ] {
        assert!(subject_has_version(versioned), "{versioned}");
    }
    for plain in [
        "[PATCH] Fix foo",
        "uuidv7 support",
        "pgv18 notes",
        "Fix the v-tables",
    ] {
        assert!(!subject_has_version(plain), "{plain}");
    }
    let s = |v: &str| Some(v.to_owned());
    assert_eq!(
        thread(&[
            (1, on(2025, 1, 1), "[PATCH v4] Avoid shifts", "inline-1"),
            (2, on(2025, 1, 2), "[PATCH v2] fix", "fix.patch"),
            (2, on(2025, 1, 2), "[PATCH v2] fix", "fix-v5.patch"),
            (3, on(2025, 1, 3), "Re: [PATCH v2] fix", "inline-1"),
        ]),
        [s("4"), s("2"), s("5"), None]
    );
    assert_eq!(
        thread_series(&[ThreadPatch {
            message: 1,
            sent_at: None,
            subject: "[PATCH v3 2/7] Fix",
            filename: "inline-1",
        }])[0]
            .part,
        None,
        "a part comes from file names only"
    );
}
