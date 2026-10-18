// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! One RFC 822 message, decoded: its Message-ID, threading headers, sender,
//! subject and date, the text body in UTF-8, and the attachments.
//!
//! MIME decoding (quoted-printable, base64, RFC 2047 words, RFC 2231
//! parameters and charsets such as ISO-8859-1, windows-1252, ISO-2022-JP or
//! GBK) is mail-parser's. This module decides what the body is: the inline
//! text/plain parts, else the text of the HTML part.

use std::collections::HashSet;
use std::sync::LazyLock;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Datelike, NaiveDateTime, TimeDelta, TimeZone, Utc};
use mail_parser::{
    Address, HeaderName, Message, MessageParser, MessagePart, MimeHeaders, PartType,
};
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::mbox;

/// A decoded message.
#[derive(Clone, Debug, Default)]
pub struct ParsedMessage {
    /// Without angle brackets or whitespace.
    pub message_id: String,
    /// The last Message-ID of In-Reply-To: MH and exmh write the address of
    /// the sender replied to before it (`Message from Tom Lane
    /// <tgl@sss.pgh.pa.us> of "…" <20593.1107191786@sss.pgh.pa.us>`).
    pub in_reply_to: Option<String>,
    /// References, oldest first, without duplicates or the message itself.
    pub references: Vec<String>,
    /// The sender's display name; never an address.
    pub from_name: String,
    /// The sender's address, lowercased.
    pub from_email: Option<String>,
    /// Decoded and unfolded.
    pub subject: String,
    /// The Date header, else the mbox `From ` line's date.
    pub sent_at: Option<DateTime<Utc>>,
    /// The text body: inline text/plain parts, else the HTML part as text.
    pub body: String,
    /// Whether [`Self::body`] came from HTML.
    pub body_is_html: bool,
    pub attachments: Vec<Attachment>,
    /// Of the bytes parsed, mbox `From ` line included.
    pub raw_sha256: Vec<u8>,
    pub raw_size: usize,
    /// The mbox `From ` line's date, if the bytes started with one.
    pub envelope_date: Option<DateTime<Utc>>,
}

/// A MIME part that isn't the body.
#[derive(Clone, Debug, Default)]
pub struct Attachment {
    /// The decoded file name, without directories.
    pub filename: Option<String>,
    /// Lowercased `type/subtype`.
    pub content_type: String,
    /// The content after transfer decoding.
    pub data: Vec<u8>,
    /// For text parts, the content decoded from its charset.
    pub text: Option<String>,
}

impl Attachment {
    /// The content as text: the decoded text of a text part, else UTF-8, else
    /// windows-1252. None for binary content (NUL bytes).
    pub fn text(&self) -> Option<String> {
        if let Some(text) = &self.text {
            return Some(text.replace("\r\n", "\n"));
        }
        if self.data.contains(&0) {
            return None;
        }
        let text = match std::str::from_utf8(&self.data) {
            Ok(text) => text.to_owned(),
            Err(_) => encoding_rs::WINDOWS_1252.decode(&self.data).0.into_owned(),
        };
        Some(text.replace("\r\n", "\n"))
    }
}

static ANGLE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<([^<>]*)>").unwrap());
static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\([^()]*\)").unwrap());
static ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+=!#$&'*/?^`{|}~-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+").unwrap()
});
static EMPTY_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(\s*\)|\[\s*\]|<\s*>|\{\s*\}").unwrap());
static ENCODED_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"=\?[^?\s]+\?[bBqQ]\?[^?\s]*\?=").unwrap());

/// Parses a raw message, which may start with an mbox `From ` line.
pub fn parse(raw: &[u8]) -> Result<ParsedMessage> {
    let raw_sha256 = Sha256::digest(raw).to_vec();
    let (envelope_date, rest) = split_envelope(raw);
    let message = MessageParser::default()
        .parse(rest)
        .ok_or_else(|| anyhow!("not an RFC 822 message"))?;

    let message_id = header_ids(&message, HeaderName::MessageId)
        .into_iter()
        .next()
        .or_else(|| message.message_id().map(normalize_message_id))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("the message has no Message-ID"))?;
    let in_reply_to = header_ids(&message, HeaderName::InReplyTo)
        .into_iter()
        .rev()
        .find(|id| *id != message_id && names_a_message(id));
    let mut seen = HashSet::from([message_id.clone()]);
    let references = header_ids(&message, HeaderName::References)
        .into_iter()
        .filter(|id| names_a_message(id) && seen.insert(id.clone()))
        .collect();
    let (from_name, from_email) = sender(&message);
    let subject = message.subject().map(clean_header).unwrap_or_default();
    let sent_at = sent_at(&message, envelope_date);
    let (body, body_is_html, attachments) = body_and_attachments(&message);

    Ok(ParsedMessage {
        message_id,
        in_reply_to,
        references,
        from_name,
        from_email,
        subject,
        sent_at,
        body,
        body_is_html,
        attachments,
        raw_sha256,
        raw_size: raw.len(),
        envelope_date,
    })
}

/// The longest token without an `@` that's a placeholder rather than an ID.
pub const PLACEHOLDER_CHARS: usize = 7;

/// Whether a token of In-Reply-To or References can name a message. Mailers
/// write short placeholders without an `@` for a message that had no
/// Message-ID, which replies to unrelated mail then share: elm's `<no.id>`,
/// `<N/A>`. Longer tokens without one, IDs that their mailers mangled
/// (Outlook's thread indexes, IDs cut at the `@`), still tell one
/// conversation from another.
pub fn names_a_message(id: &str) -> bool {
    id.contains('@') || id.chars().count() > PLACEHOLDER_CHARS
}

/// A Message-ID without angle brackets, whitespace or control characters, as
/// messages store it.
pub fn normalize_message_id(id: &str) -> String {
    let id = id.trim();
    let id = id.strip_prefix('<').unwrap_or(id);
    let id = id.strip_suffix('>').unwrap_or(id);
    id.chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect()
}

/// The Message-ID of a raw message from its headers alone, cheaply: for
/// skipping messages already stored.
pub fn header_message_id(raw: &[u8]) -> Option<String> {
    let (_, rest) = split_envelope(raw);
    let message = MessageParser::default().parse_headers(rest)?;
    header_ids(&message, HeaderName::MessageId)
        .into_iter()
        .next()
        .or_else(|| message.message_id().map(normalize_message_id))
        .filter(|id| !id.is_empty())
}

/// The mbox `From ` line's date, and the message after the line.
fn split_envelope(raw: &[u8]) -> (Option<DateTime<Utc>>, &[u8]) {
    if !raw.starts_with(b"From ") {
        return (None, raw);
    }
    match raw.iter().position(|&b| b == b'\n') {
        Some(end) => (mbox::from_line_date(&raw[..end]), &raw[end + 1..]),
        None => (mbox::from_line_date(raw), &[]),
    }
}

/// The Message-IDs in a header, in order: `<...>` tokens outside comments,
/// else the whole value if it is one token with an `@`.
fn header_ids(message: &Message<'_>, name: HeaderName<'static>) -> Vec<String> {
    let raw = match message.header_raw(name.clone()) {
        Some(raw) => raw.to_owned(),
        None => match message.header(name) {
            Some(value) => value
                .as_text_list()
                .map(|ids| {
                    ids.iter()
                        .map(|id| format!("<{id}>"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            None => return Vec::new(),
        },
    };
    extract_ids(&raw)
}

/// Message-IDs in a header value.
pub fn extract_ids(value: &str) -> Vec<String> {
    let mut text = value.to_owned();
    for _ in 0..3 {
        let stripped = COMMENT.replace_all(&text, " ").into_owned();
        if stripped == text {
            break;
        }
        text = stripped;
    }
    let mut ids: Vec<String> = ANGLE_ID
        .captures_iter(&text)
        .map(|caps| normalize_message_id(&caps[1]))
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        let token = text.trim();
        if !token.is_empty() && !token.contains(char::is_whitespace) && token.contains('@') {
            ids.push(normalize_message_id(token));
        }
    }
    ids
}

/// The sender's display name, addresses kept, and lowercased address.
fn sender(message: &Message<'_>) -> (String, Option<String>) {
    let addr = message.from().and_then(first_address);
    let (name, email) = match addr {
        Some((name, email)) => (name, email),
        None => {
            let raw = message.header_raw(HeaderName::From).unwrap_or_default();
            let email = ADDRESS.find(raw).map(|m| m.as_str().to_owned());
            (String::new(), email)
        }
    };
    let email = email
        .map(|e| e.trim().trim_matches(['<', '>']).to_lowercase())
        .filter(|e| !e.is_empty());
    (display_name(&name, email.as_deref()), email)
}

fn first_address(address: &Address<'_>) -> Option<(String, Option<String>)> {
    let addr = address.first()?;
    Some((
        addr.name.as_deref().unwrap_or_default().to_owned(),
        addr.address.as_deref().map(str::to_owned),
    ))
}

/// A display name as written, addresses kept, without quotes or stray
/// whitespace; the address if nothing is left.
pub fn display_name(name: &str, email: Option<&str>) -> String {
    let name = trim_name(clean_header(&decode_encoded_words(name)));
    if name.chars().any(char::is_alphanumeric) {
        return name;
    }
    match email.filter(|e| !e.is_empty()) {
        Some(email) => email.to_owned(),
        None => "unknown".to_owned(),
    }
}

/// A name without empty brackets, or quotes and commas around it.
fn trim_name(mut name: String) -> String {
    loop {
        let trimmed = EMPTY_BRACKETS.replace_all(&name, "");
        let trimmed = trimmed
            .trim_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ','));
        let trimmed = clean_header(trimmed);
        if trimmed == name {
            return name;
        }
        name = trimmed;
    }
}

/// Header text unfolded: control characters and runs of whitespace become one
/// space.
fn clean_header(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// RFC 2047 encoded words that a client left inside a quoted parameter.
fn decode_encoded_words(text: &str) -> String {
    if !ENCODED_WORD.is_match(text) {
        return text.to_owned();
    }
    let header = format!("Subject: {}\n\n", text.replace(['\r', '\n'], " "));
    MessageParser::default()
        .parse_headers(header.as_bytes())
        .and_then(|m| m.subject().map(str::to_owned))
        .unwrap_or_else(|| text.to_owned())
}

/// The Date header if it is plausible next to the mbox date, else the mbox
/// date.
fn sent_at(message: &Message<'_>, envelope: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    let header = message
        .date()
        .filter(|d| d.is_valid())
        .and_then(|d| Utc.timestamp_opt(d.to_timestamp(), 0).single())
        .or_else(|| {
            message
                .header_raw(HeaderName::Date)
                .and_then(parse_date_leniently)
        })
        .filter(|d| (1990..=2100).contains(&d.year()));
    match (header, envelope) {
        (Some(date), Some(envelope))
            if date > envelope + TimeDelta::days(2) || date < envelope - TimeDelta::days(400) =>
        {
            Some(envelope)
        }
        (Some(date), _) => Some(date),
        (None, envelope) => envelope,
    }
}

/// Dates that mail-parser rejects: RFC 2822 with junk, ISO 8601 and friends.
pub fn parse_date_leniently(raw: &str) -> Option<DateTime<Utc>> {
    let text = COMMENT.replace_all(raw, " ");
    let text = clean_header(&text);
    if let Ok(date) = DateTime::parse_from_rfc2822(&text) {
        return Some(date.with_timezone(&Utc));
    }
    for format in [
        "%a, %d %b %Y %H:%M:%S %z",
        "%d %b %Y %H:%M:%S %z",
        "%a %b %d %H:%M:%S %Y %z",
        "%a, %d %b %Y %H:%M %z",
        "%Y-%m-%d %H:%M:%S %z",
        "%Y-%m-%dT%H:%M:%S%z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
    ] {
        if let Ok(date) = DateTime::parse_from_str(&text, format) {
            return Some(date.with_timezone(&Utc));
        }
    }
    let without_zone = text
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c.is_whitespace())
        .trim();
    for format in [
        "%a, %d %b %Y %H:%M:%S",
        "%d %b %Y %H:%M:%S",
        "%a %b %d %H:%M:%S %Y",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(date) = NaiveDateTime::parse_from_str(without_zone, format) {
            return Some(date.and_utc());
        }
    }
    None
}

/// The body, whether it came from HTML, and the attachments.
fn body_and_attachments(message: &Message<'_>) -> (String, bool, Vec<Attachment>) {
    let mut texts = Vec::new();
    let mut html = None;
    let mut attachments = Vec::new();
    let mut taken = HashSet::new();
    for &id in &message.text_body {
        let Some(part) = message.parts.get(id as usize) else {
            continue;
        };
        match &part.body {
            PartType::Text(text) if part.attachment_name().is_none() => {
                texts.push(text.as_ref());
                taken.insert(id);
            }
            PartType::Text(_) => {
                attachments.push(attachment(part));
                taken.insert(id);
            }
            PartType::Html(text) if html.is_none() => html = Some(text.as_ref()),
            _ => {}
        }
    }
    if texts.is_empty() && html.is_none() {
        html = message
            .html_body
            .iter()
            .filter_map(|&id| message.parts.get(id as usize))
            .find_map(|part| match &part.body {
                PartType::Html(text) => Some(text.as_ref()),
                _ => None,
            });
    }
    for &id in &message.attachments {
        if taken.contains(&id) {
            continue;
        }
        let Some(part) = message.parts.get(id as usize) else {
            continue;
        };
        if matches!(part.body, PartType::Multipart(_) | PartType::Message(_)) {
            continue;
        }
        attachments.push(attachment(part));
    }
    let (body, is_html) = if !texts.is_empty() {
        (texts.join("\n"), false)
    } else if let Some(html) = html {
        (mail_parser::decoders::html::html_to_text(html), true)
    } else {
        (String::new(), false)
    };
    (sanitize(&body), is_html, attachments)
}

fn attachment(part: &MessagePart<'_>) -> Attachment {
    let content_type = part
        .content_type()
        .map(|ct| match ct.subtype() {
            Some(subtype) => format!("{}/{}", ct.ctype(), subtype),
            None => ct.ctype().to_owned(),
        })
        .unwrap_or_else(|| "text/plain".to_owned())
        .to_lowercase();
    let filename = part
        .attachment_name()
        .map(file_name)
        .filter(|n| !n.is_empty());
    let (data, text) = match &part.body {
        PartType::Text(text) | PartType::Html(text) => {
            (text.as_bytes().to_vec(), Some(text.as_ref().to_owned()))
        }
        PartType::Binary(bytes) | PartType::InlineBinary(bytes) => (bytes.to_vec(), None),
        PartType::Message(message) => (message.raw_message.to_vec(), None),
        PartType::Multipart(_) => (Vec::new(), None),
    };
    Attachment {
        filename,
        content_type,
        data,
        text,
    }
}

/// An attachment's name decoded, without directories or quotes.
fn file_name(name: &str) -> String {
    let name = decode_encoded_words(name);
    let name = name.trim().trim_matches(['"', '\'']).trim();
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    clean_header(base)
}

/// Text as stored: LF line ends and no NUL characters, which PostgreSQL text
/// can't hold.
pub fn sanitize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\0', "")
}

#[cfg(test)]
mod tests;
