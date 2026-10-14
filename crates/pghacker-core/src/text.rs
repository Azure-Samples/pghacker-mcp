//! Text helpers: truncation on character boundaries, whitespace-insensitive
//! quote checks for citations, and recognizing the email redaction that
//! ingest once applied.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

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

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+=-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}").unwrap()
});

/// `text` as ingest once stored it: email addresses replaced by `[email]`,
/// Message-IDs in URLs (`/message-id/<id>`, `postgr.es/m/<id>`), which look
/// like addresses, kept. Text is now stored as written; this recognizes the
/// text stored before, to give it its addresses back.
pub fn redact_emails(text: &str) -> Cow<'_, str> {
    EMAIL.replace_all(text, |caps: &regex::Captures<'_>| {
        let m = caps.get(0).unwrap();
        let before = &text[..m.start()];
        if before.ends_with('/')
            || before.ends_with("message-id:")
            || before.ends_with("Message-ID: <")
        {
            m.as_str().to_owned()
        } else {
            "[email]".to_owned()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn redacts_addresses_but_not_message_id_urls() {
        let text =
            "Tom Lane <tgl@sss.pgh.pa.us> wrote. See https://postgr.es/m/CA+abc=d@mail.gmail.com";
        assert_eq!(
            redact_emails(text),
            "Tom Lane <[email]> wrote. See https://postgr.es/m/CA+abc=d@mail.gmail.com"
        );
    }
}
