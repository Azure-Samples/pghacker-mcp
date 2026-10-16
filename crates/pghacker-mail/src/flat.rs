// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The archive's flat thread view, `/message-id/flat/<id>`: the whole thread
//! on one page, oldest message first, each with a header table whose
//! `Message-ID:` row links to the message.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::parse::normalize_message_id;

static MESSAGE_ID_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<th[^>]*>\s*Message-ID:\s*</th>\s*<td[^>]*>\s*<a\s[^>]*?href="[^"]*?/message-id/([^"]+)"[^>]*>(.*?)</a>"#,
    )
    .unwrap()
});

/// The Message-IDs of the messages on a flat thread page, in page order,
/// without duplicates.
pub fn message_ids(html: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for caps in MESSAGE_ID_ROW.captures_iter(html) {
        let text = html_escape::decode_html_entities(caps[2].trim()).into_owned();
        let id = if text.is_empty() || text.contains('<') {
            normalize_message_id(&percent_decode(&caps[1]))
        } else {
            normalize_message_id(&text)
        };
        if !id.is_empty() && seen.insert(id.clone()) {
            ids.push(id);
        }
    }
    ids
}

/// Whether `html` is a thread page rather than an error page.
pub fn looks_like_flat_page(html: &str) -> bool {
    MESSAGE_ID_ROW.is_match(html)
}

/// `text` with `%XX` escapes decoded; invalid escapes stay as they are.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(high * 16 + low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"
<h1 id="flatMsgSubject" class="subject">Re: A thread</h1>
<a name="root-1@example.org"></a>
<table class="table-sm table-responsive message-header">
 <tr>
  <th scope="row" style="width: 10%">From:</th>
  <td>Alice Example &lt;alice(at)example(dot)org&gt;</td>
 </tr>
 <tr>
  <th scope="row">Message-ID:</th>
  <td><a href="/message-id/root-1%40example.org">root-1@example.org</a></td>
 </tr>
</table>
<table class="table-sm table-responsive message-header">
 <tr>
  <th scope="row">Message-ID:</th>
  <td><a href="/message-id/CA%2Bx%3Dy%2Fz%40mail.example.org">CA+x=y/z@mail.example.org</a></td>
 </tr>
 <tr>
  <th scope="row">Message-ID:</th>
  <td><a href="/message-id/a%26b%40example.org">a&amp;b@example.org</a></td>
 </tr>
 <tr>
  <th scope="row">Message-ID:</th>
  <td><a href="/message-id/root-1%40example.org">root-1@example.org</a></td>
 </tr>
</table>"##;

    #[test]
    fn lists_message_ids_in_order_once() {
        assert_eq!(
            message_ids(PAGE),
            [
                "root-1@example.org",
                "CA+x=y/z@mail.example.org",
                "a&b@example.org"
            ]
        );
        assert!(looks_like_flat_page(PAGE));
        assert!(!looks_like_flat_page(
            "<html><title>Not found</title></html>"
        ));
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%2Bb%3D%40c%zz%4"), "a+b=@c%zz%4");
    }
}
