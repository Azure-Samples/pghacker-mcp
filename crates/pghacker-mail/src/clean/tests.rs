// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::*;

#[test]
fn drops_quotes_and_attributions() {
    let body = "Hi,\n\nOn Mon, Jun 1, 2026 at 9:00 AM Bob Example <bob@example.org> wrote:\n> Could you rebase?\n>\n> Thanks.\n\nRebased.  See below.\n\nBob Example <bob@example.org> writes:\n> Another point.\n\nAgreed with that.\n";
    assert_eq!(
        clean_body(body),
        "Hi,\n\nRebased.  See below.\n\nAgreed with that."
    );
}

#[test]
fn drops_wrapped_and_message_id_attributions() {
    let body = "On Mon, Jun 1, 2026 at 9:00 AM Bob Example <bob@example.org>\nwrote:\n> quoted\nMine.\n\nAt Sun, 31 May 2026 17:50:08 +0900, Carol <carol@example.org> wrote in\n<abc@mail.example.org>\n> quoted\n\nAlso mine.\n\nAm 01.06.2026 um 11:38 schrieb Dave <dave@example.org>:\n\n> quoted\n\nStill mine.";
    assert_eq!(clean_body(body), "Mine.\n\nAlso mine.\n\nStill mine.");
}

#[test]
fn keeps_lines_that_merely_end_with_a_colon() {
    let body = "Here is what I propose:\n\n1. Do this.\n2. Do that.";
    assert_eq!(clean_body(body), body);
}

#[test]
fn drops_signatures() {
    assert_eq!(
        clean_body("Thanks!\n\n-- \nAlice Example\nhttps://example.org/\n"),
        "Thanks!"
    );
    assert_eq!(clean_body("Thanks!\n--\nAlice\n"), "Thanks!");
    // A "--" with a long tail is not a signature.
    let long = format!("Before.\n--\n{}", "line\n".repeat(10));
    assert!(clean_body(&long).contains("Before.\n--\nline"));
}

#[test]
fn drops_inline_diffs_and_format_patch_headers() {
    let body = "\
I fixed it like this:

From 0123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001
From: Alice Example <alice@example.org>
Date: Tue, 2 Jun 2026 10:00:00 +0200
Subject: [PATCH] Fix

The commit message stays.
---
 src/backend/foo.c | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)

diff --git a/src/backend/foo.c b/src/backend/foo.c
index 1111111..2222222 100644
--- a/src/backend/foo.c
+++ b/src/backend/foo.c
@@ -1,3 +1,3 @@
 int a;
-int b;
+int c;
 int d;
-- 
2.43.0

What do you think?
";
    assert_eq!(
        clean_body(body),
        "I fixed it like this:\n\nThe commit message stays.\n\nWhat do you think?"
    );
}

#[test]
fn keeps_code_that_is_not_a_diff() {
    let body = "Maybe:\n\n+ a new line\n- an old line\n\nRight?";
    assert_eq!(clean_body(body), body);
}

#[test]
fn cuts_forwarded_and_outlook_quotes_and_list_footers() {
    let body = "Top post.\n\nFrom: Bob Example <bob@example.org>\nSent: Monday, June 1, 2026 9:00 AM\nTo: pgsql-hackers\nSubject: Re: thing\n\nThe whole earlier message.";
    assert_eq!(clean_body(body), "Top post.");
    let body = "FYI.\n\n---------- Forwarded message ---------\nFrom: Bob\n\nForwarded text.";
    assert_eq!(clean_body(body), "FYI.");
    let body = "Old-style message.\n\n--\nSent via pgsql-hackers mailing list (pgsql-hackers@lists.example.org)\nTo make changes to your subscription:\nhttp://www.postgresql.org/mailpref/pgsql-hackers\n";
    assert_eq!(clean_body(body), "Old-style message.");
    // A format-patch header is not an Outlook quote.
    let body = "Text.\n\nFrom: Alice <alice@example.org>\nDate: Tue, 2 Jun 2026\nSubject: [PATCH] x\n\nMore text.";
    assert!(clean_body(body).contains("More text."));
}

#[test]
fn keeps_the_authors_words_verbatim() {
    let body = "On Tue, Bob wrote:\n> quoted\n\nI think  we should not\ncommit this yet.\r\n";
    let clean = clean_body(body);
    assert_eq!(clean, "I think  we should not\ncommit this yet.");
    assert!(pghacker_core::text::quote_in(
        body,
        "I think we should not commit this yet."
    ));
    assert!(pghacker_core::text::quote_in(
        &clean,
        "I think we should not commit this yet."
    ));
}

#[test]
fn collapses_blank_lines_and_empty_bodies() {
    assert_eq!(clean_body("\n\n\nA\n\n\n\nB\n\n"), "A\n\nB");
    assert_eq!(clean_body("> only a quote\n"), "");
    assert_eq!(clean_body(""), "");
}
