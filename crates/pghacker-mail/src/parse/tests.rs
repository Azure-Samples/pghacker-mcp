// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::*;

const MULTIPART: &[u8] = include_bytes!("../../tests/fixtures/multipart_patch.eml");
const LATIN1: &[u8] = include_bytes!("../../tests/fixtures/latin1_no_date.eml");
const HTML: &[u8] = include_bytes!("../../tests/fixtures/html_only.eml");
const COMPRESSED: &[u8] = include_bytes!("../../tests/fixtures/compressed_patches.eml");

#[test]
fn parses_a_multipart_message_with_folded_headers_and_patches() {
    let msg = parse(MULTIPART).unwrap();
    assert_eq!(msg.message_id, "CAB+x=y/z@mail.example.org");
    assert_eq!(msg.in_reply_to.as_deref(), Some("parent-2@example.org"));
    assert_eq!(
        msg.references,
        ["root-1@example.org", "parent-2@example.org"]
    );
    assert_eq!(msg.from_name, "Alice Example");
    assert_eq!(msg.from_email.as_deref(), Some("alice@example.org"));
    assert_eq!(
        msg.subject,
        "Re: [PATCH v3] Fix the frobnicator in src/backend/utils/adt/frob.c"
    );
    assert_eq!(
        msg.sent_at.unwrap().to_rfc3339(),
        "2026-06-02T08:30:00+00:00"
    );
    assert_eq!(
        msg.envelope_date.unwrap().to_rfc3339(),
        "2026-06-02T08:31:00+00:00"
    );
    // Quoted-printable, UTF-8.
    assert!(
        msg.body.contains("The café test passes now"),
        "{}",
        msg.body
    );
    assert!(msg.body.contains("\n-- \nAlice"));
    assert!(!msg.body_is_html);
    let names: Vec<_> = msg
        .attachments
        .iter()
        .map(|a| a.filename.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        names,
        [
            "v3-0001-Fix-the-frobnicator.patch",
            "v3-0002-Add-tests.patch",
            "signature.asc"
        ]
    );
    assert_eq!(msg.attachments[0].content_type, "text/x-patch");
    assert_eq!(msg.attachments[1].content_type, "application/octet-stream");
    let first = msg.attachments[0].text().unwrap();
    assert!(first.starts_with("From 89abcdef"));
    assert!(first.contains("+\tif (x < 0)\n"));
    let second = msg.attachments[1].text().unwrap();
    assert!(second.contains("+SELECT frob(2);"));
    assert_eq!(msg.raw_size, MULTIPART.len());
    assert_eq!(msg.raw_sha256, Sha256::digest(MULTIPART).to_vec());
}

#[test]
fn decodes_latin1_and_falls_back_to_the_mbox_date() {
    let msg = parse(LATIN1).unwrap();
    assert_eq!(msg.from_name, "André Exemple");
    assert_eq!(msg.subject, "Café crème");
    assert_eq!(msg.body.trim_end(), "Bonjour, un café crème.");
    assert_eq!(
        msg.sent_at.unwrap().to_rfc3339(),
        "2026-06-03T12:00:00+00:00"
    );
    assert!(msg.in_reply_to.is_none() && msg.references.is_empty());
}

#[test]
fn falls_back_to_html_text() {
    let msg = parse(HTML).unwrap();
    assert!(msg.body_is_html);
    assert!(msg.body.contains("It’s “quoted”."), "{:?}", msg.body);
    assert!(msg.body.contains("Second"));
    assert!(!msg.body.contains("<p>"));
    // No display name: the address's local part.
    assert_eq!(msg.from_name, "bob");
    assert_eq!(msg.from_email.as_deref(), Some("bob@example.org"));
}

#[test]
fn decodes_encoded_attachment_names() {
    let msg = parse(COMPRESSED).unwrap();
    let names: Vec<_> = msg
        .attachments
        .iter()
        .filter_map(|a| a.filename.as_deref())
        .collect();
    assert!(names.contains(&"v1-0001-encoded-name.patch"), "{names:?}");
    assert!(names.contains(&"v4-0001-Rename.patch.gz"), "{names:?}");
}

#[test]
fn message_ids_are_normalized() {
    assert_eq!(
        normalize_message_id(" <a b@example.org> "),
        "ab@example.org"
    );
    assert_eq!(normalize_message_id("x@example.org"), "x@example.org");
    assert_eq!(
        extract_ids("<a@example.org> (Bob's message of <b@example.org>)"),
        ["a@example.org"]
    );
    assert_eq!(
        extract_ids("\n <a@example.org>\n\t<b@example.org> "),
        ["a@example.org", "b@example.org"]
    );
    assert_eq!(extract_ids("c@example.org"), ["c@example.org"]);
    assert!(extract_ids("Your message of yesterday").is_empty());
    let raw = b"Message-Id: <  spaced@example.org >\nIn-Reply-To: <spaced@example.org>\nReferences: <r@example.org> <r@example.org>\n\nx";
    let msg = parse(raw).unwrap();
    assert_eq!(msg.message_id, "spaced@example.org");
    assert_eq!(msg.in_reply_to, None, "a message doesn't reply to itself");
    assert_eq!(msg.references, ["r@example.org"]);
    assert_eq!(
        header_message_id(MULTIPART).as_deref(),
        Some("CAB+x=y/z@mail.example.org")
    );
}

#[test]
fn in_reply_to_takes_the_message_id_after_an_address() {
    let parse_headers = |headers: &str| {
        let raw = format!("Message-ID: <reply@example.org>\n{headers}\n\nx\n");
        let msg = parse(raw.as_bytes()).unwrap();
        (msg.in_reply_to, msg.references)
    };
    // exmh and MH-E write the sender replied to before the Message-ID.
    let (parent, _) = parse_headers(
        "In-Reply-To: Message from Tom Lane <tom@example.org> \n\tof \"Tue, 24 Nov 1998 11:17:58 EST.\" <26279.911924278@sss.pgh.pa.us> ",
    );
    assert_eq!(parent.as_deref(), Some("26279.911924278@sss.pgh.pa.us"));
    // nmh's form without one keeps the address: the store knows senders.
    let (parent, _) =
        parse_headers("In-Reply-To: Your message of \"Mon, 14 Apr 2003\" <a@example.org>");
    assert_eq!(parent.as_deref(), Some("a@example.org"));
    // elm's placeholder for a message without a Message-ID, and others.
    let (parent, references) = parse_headers(
        "In-Reply-To: <no.id> from \"nolan\" at Jul 09, 2003 01:29:06 PM\nReferences: <N/A> <r@example.org> <no.id>",
    );
    assert_eq!(parent, None);
    assert_eq!(references, ["r@example.org"]);
    // IDs that mailers mangled still name their conversation.
    let (parent, references) = parse_headers(
        "In-Reply-To: <ff3a0357-5b79-4fc7-8f47-663865d777cb.>\nReferences: <AckzBS5PNwDGidYbSuSDgtnRwVhKMA==> <ff3a0357-5b79-4fc7-8f47-663865d777cb.>",
    );
    assert_eq!(
        parent.as_deref(),
        Some("ff3a0357-5b79-4fc7-8f47-663865d777cb.")
    );
    assert_eq!(
        references,
        [
            "AckzBS5PNwDGidYbSuSDgtnRwVhKMA==",
            "ff3a0357-5b79-4fc7-8f47-663865d777cb."
        ]
    );
    assert!(names_a_message("x@y") && names_a_message("12345678"));
    assert!(!names_a_message("no.id") && !names_a_message("N/A") && !names_a_message("unknown"));
}

#[test]
fn names_lose_their_addresses() {
    let parse_from = |from: &str| {
        let raw = format!("From: {from}\nMessage-ID: <n@example.org>\n\nbody\n");
        let msg = parse(raw.as_bytes()).unwrap();
        (msg.from_name, msg.from_email)
    };
    assert_eq!(
        parse_from("\"dave@example.org\" <dave@example.org>"),
        ("dave".to_owned(), Some("dave@example.org".to_owned()))
    );
    assert_eq!(
        parse_from("Dave Example (dave@example.org) <dave@example.org>").0,
        "Dave Example"
    );
    assert_eq!(parse_from("<dave@example.org>").0, "dave");
    assert_eq!(display_name("dave@example.org", None), "dave");
    assert_eq!(
        parse_from("'Eve Example' <EVE@Example.ORG>").1.as_deref(),
        Some("eve@example.org")
    );
    assert_eq!(display_name("", None), "unknown");
}

#[test]
fn tolerates_junk_dates() {
    let with_date = |date: &str, envelope: bool| {
        let mut raw = String::new();
        if envelope {
            raw.push_str("From a@example.org Fri Jun 05 10:00:00 2026\n");
        }
        raw.push_str(&format!("Date: {date}\nMessage-ID: <d@example.org>\n\nx\n"));
        parse(raw.as_bytes())
            .unwrap()
            .sent_at
            .map(|d| d.to_rfc3339())
    };
    assert_eq!(
        with_date("Fri, 5 Jun 2026 11:59:00 +0200 (CEST)", true).as_deref(),
        Some("2026-06-05T09:59:00+00:00")
    );
    assert_eq!(
        with_date("2026-06-05 09:00:00 +0000", false).as_deref(),
        Some("2026-06-05T09:00:00+00:00")
    );
    // Junk, or a clock years off: the mbox date.
    assert_eq!(
        with_date("sometime last week", true).as_deref(),
        Some("2026-06-05T10:00:00+00:00")
    );
    assert_eq!(
        with_date("Fri, 5 Jun 2037 10:00:00 +0000", true).as_deref(),
        Some("2026-06-05T10:00:00+00:00")
    );
    assert_eq!(with_date("sometime last week", false), None);
}

#[test]
fn a_message_needs_a_message_id() {
    let error = parse(b"From: a@example.org\nSubject: x\n\nbody\n").unwrap_err();
    assert!(error.to_string().contains("Message-ID"));
    assert!(parse(b"").is_err());
}

#[test]
fn inline_text_parts_with_names_are_attachments() {
    let raw = b"Message-ID: <i@example.org>\nContent-Type: multipart/mixed; boundary=b\n\n--b\nContent-Type: text/plain\n\nThe body.\n--b\nContent-Type: text/plain\nContent-Disposition: inline; filename=\"fix.diff\"\n\ndiff --git a/x b/x\n--b\nContent-Type: text/plain\n\nA footer part.\n--b--\n";
    let msg = parse(raw).unwrap();
    assert!(msg.body.contains("The body."));
    assert!(msg.body.contains("A footer part."));
    assert!(!msg.body.contains("diff --git"));
    assert_eq!(msg.attachments.len(), 1);
    assert_eq!(msg.attachments[0].filename.as_deref(), Some("fix.diff"));
}

#[test]
fn removes_nul_characters() {
    let raw = b"Message-ID: <z@example.org>\nSubject: a\0b\n\nbody\0text\n";
    let msg = parse(raw).unwrap();
    assert!(!msg.body.contains('\0'));
    assert!(!msg.subject.contains('\0'));
}
