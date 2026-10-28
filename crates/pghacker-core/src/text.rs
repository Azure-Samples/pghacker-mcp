// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Text helpers: presentation-control filtering, truncation on character
//! boundaries and whitespace-insensitive quote checks for citations.

/// Removes C0/C1 controls and DEL except tab and LF, and Unicode bidi
/// controls (directional marks, embeddings, overrides, pops and isolates).
/// Removing CR normalizes CRLF to LF and prevents standalone CR overwrites.
/// Other Unicode, HTML and Markdown stay as written: this is not a renderer's
/// output encoding or a defense against instructions embedded in text.
pub fn strip_presentation_controls(text: &mut String) {
    text.retain(|c| {
        (!c.is_control() || matches!(c, '\t' | '\n'))
            && !matches!(
                c,
                '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
    });
}

/// The Message-ID a commit's Discussion trailer names, as messages store it
/// (no angle brackets): a postgr.es/m or postgresql.org/message-id link, or
/// `<id>`.
pub fn discussion_message_id(value: &str) -> Option<String> {
    let v = value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches(['>', '.', ',', ';']);
    let rest = [
        "postgr.es/m/",
        "postgresql.org/message-id/flat/",
        "postgresql.org/message-id/",
    ]
    .iter()
    .find_map(|p| v.find(p).map(|i| &v[i + p.len()..]));
    let id = match rest {
        Some(rest) => rest.split(['/', '?', '#']).next().unwrap_or(""),
        None if v.contains('@') && !v.contains('/') => v,
        None => return None,
    };
    let id = percent_decode(id);
    let id = id.trim_start_matches('<').trim_end_matches('>');
    (id.contains('@')).then(|| id.to_owned())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// At most `max` characters of `text`, cut on a character boundary.
pub fn truncate_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// `text` with runs of whitespace, and the `>` that mark quoted lines,
/// collapsed to single spaces: how citations compare quotes with messages.
pub fn normalize_for_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let line = line.trim_start();
        let line = line.trim_start_matches(|c: char| c == '>' || c.is_whitespace());
        for word in line.split_whitespace() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
        }
    }
    out.replace(['\u{2018}', '\u{2019}'], "'")
        .replace(['\u{201c}', '\u{201d}'], "\"")
}

/// Whether `quote` appears verbatim in `body`, ignoring whitespace and quote
/// markers. Quotes shorter than 12 characters never count: they prove
/// nothing.
pub fn quote_in(body: &str, quote: &str) -> bool {
    let quote = normalize_for_quote(quote);
    if quote.chars().count() < 12 {
        return false;
    }
    normalize_for_quote(body).contains(&quote)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_controls_leave_only_tab_and_lf() {
        for code in (0..=0x1f).chain(0x7f..=0x9f) {
            let control = char::from_u32(code).unwrap();
            let mut text = format!("before{control}after");
            strip_presentation_controls(&mut text);
            let expected = match control {
                '\t' | '\n' => format!("before{control}after"),
                _ => "beforeafter".to_owned(),
            };
            assert_eq!(text, expected, "U+{code:04X}");
        }
        let mut text = "one\r\ntwo\rthree".to_owned();
        strip_presentation_controls(&mut text);
        assert_eq!(text, "one\ntwothree");
    }

    #[test]
    fn bidi_controls_are_removed_including_marks_and_isolates() {
        for control in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            let mut text = format!("before{control}after");
            strip_presentation_controls(&mut text);
            assert_eq!(text, "beforeafter", "U+{:04X}", control as u32);
        }
    }

    #[test]
    fn presentation_filter_preserves_markup_code_and_other_unicode() {
        let original = concat!(
            "\tif (x < 0)\n+SELECT 'a,b';\n",
            "<script>sample</script><img src=x onerror=sample><svg onload=sample></svg>\n",
            "&lt;script&gt;sample&lt;/script&gt;\n",
            "![image](https://example.invalid/pixel) [link](javascript:sample)\n",
            "[data](data:text/html,sample) [file](file:///sample)\n",
            "![reference][image]\n[image]: https://example.invalid/pixel\n",
            "caf\u{e9} e\u{301} \u{05e9}\u{05dc}\u{05d5}\u{05dd} ",
            "\u{1f469}\u{200d}\u{1f4bb} \u{200b}\u{fe0f}",
        );
        let mut text = original.to_owned();
        strip_presentation_controls(&mut text);
        assert_eq!(text, original);
    }

    #[test]
    fn presentation_filter_is_idempotent_for_empty_and_hostile_text() {
        for original in [
            "",
            "\0\u{1b}\u{9b}\u{202e}\u{2069}",
            "\u{1b}[2J\u{1b}]8;;https://example.invalid\u{1b}\\link\u{1b}]8;;\u{1b}\\",
            "\u{1b}]52;c;c2FtcGxl\u{7}\u{9d}52;c;c2FtcGxl\u{9c}",
            "\u{2066}a\u{202e}b\u{202c}c\u{2069}\r\n\ttext",
        ] {
            let mut text = original.to_owned();
            strip_presentation_controls(&mut text);
            let once = text.clone();
            strip_presentation_controls(&mut text);
            assert_eq!(text, once);
            assert!(
                text.chars()
                    .all(|c| !c.is_control() || matches!(c, '\t' | '\n'))
            );
        }
    }

    #[test]
    fn discussion_links_name_messages() {
        assert_eq!(
            discussion_message_id("https://postgr.es/m/CAH2-Wz=4CJK9@mail.gmail.com").as_deref(),
            Some("CAH2-Wz=4CJK9@mail.gmail.com")
        );
        assert_eq!(
            discussion_message_id("https://www.postgresql.org/message-id/flat/abc%40example.org")
                .as_deref(),
            Some("abc@example.org")
        );
        assert_eq!(
            discussion_message_id("<20190101.123@example.org>").as_deref(),
            Some("20190101.123@example.org")
        );
        assert_eq!(discussion_message_id("https://example.org/thread"), None);
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("hi", 5), "hi");
    }

    #[test]
    fn quotes_match_across_wrapping_and_markers() {
        let body =
            "On Monday Tom wrote:\n> I think we should not\n> commit this patch yet.\n\nAgreed.";
        assert!(quote_in(
            body,
            "I think we should not commit this patch yet."
        ));
        assert!(!quote_in(body, "we should commit this patch"));
        assert!(!quote_in(body, "Agreed."));
    }
}
