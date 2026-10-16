// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Subjects: the form threads display, and the key that matches a reply that
//! lost its threading headers with its thread.

use std::sync::LazyLock;

use regex::Regex;

/// Reply and forward prefixes, in the languages pgsql-hackers sees.
static REPLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(?:re|aw|sv|antw|odp|vs|ynt|fwd?|wg|tr|rif|回复|回覆|答复|转发|轉寄)\s*(?:\[\d+\]|\(\d+\)|\^\d+)?\s*[:：]\s*",
    )
    .unwrap()
});

/// Mailing list tags such as `[HACKERS]`.
static LIST_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*\[(?:hackers|pgsql-[a-z]+|general|bugs|committers|docs|performance)\]\s*")
        .unwrap()
});

/// Patch and status tags: `[PATCH v3 2/7]`, `[RFC]`, `[POC]`, `WIP:` and so on.
static PATCH_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(?:\[[^\]]*\b(?:patch(?:es)?|rfc|poc|wip|proposal|draft|v\d+|bug|doc|docs|idea)\b[^\]]*\]|(?:wip|rfc|poc|proposal|patch|idea)\s*[:：])\s*",
    )
    .unwrap()
});

/// Whether the subject says the message replies to or forwards another.
pub fn is_reply(subject: &str) -> bool {
    let mut text = subject;
    loop {
        if REPLY.is_match(text) {
            return true;
        }
        match LIST_TAG.find(text) {
            Some(tag) => text = &text[tag.end()..],
            None => return false,
        }
    }
}

/// The subject without reply and forward prefixes or list tags, with
/// whitespace collapsed: what a thread displays.
pub fn display(subject: &str) -> String {
    let mut text = subject.trim();
    loop {
        let before = text.len();
        for pattern in [&*REPLY, &*LIST_TAG] {
            if let Some(found) = pattern.find(text) {
                text = &text[found.end()..];
            }
        }
        if text.len() == before {
            break;
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The display form without patch tags, lowercased and without trailing
/// punctuation: equal keys mean the same topic. Empty when nothing
/// distinctive is left.
pub fn key(subject: &str) -> String {
    let display = display(subject);
    let mut text = display.as_str();
    loop {
        let before = text.len();
        for pattern in [&*PATCH_TAG, &*REPLY, &*LIST_TAG] {
            if let Some(found) = pattern.find(text) {
                text = &text[found.end()..];
            }
        }
        if text.len() == before {
            break;
        }
    }
    let key = text
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let key = key.trim_end_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace());
    if key.chars().filter(|c| c.is_alphanumeric()).count() < 4 {
        String::new()
    } else {
        key.to_owned()
    }
}

/// Whether a key says enough to match threads by: three words and a dozen
/// letters or digits. "fix typo" does not.
pub fn is_distinctive(key: &str) -> bool {
    key.split_whitespace().count() >= 3 && key.chars().filter(|c| c.is_alphanumeric()).count() >= 12
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_reply_prefixes_and_list_tags() {
        assert_eq!(
            display("Re: RE: [HACKERS] Re: Fix the planner "),
            "Fix the planner"
        );
        assert_eq!(display("AW: Sv:  Two   spaces"), "Two spaces");
        assert_eq!(display("Re[2]: x"), "x");
        assert_eq!(display("回复：中文"), "中文");
        assert_eq!(display("Fwd: [PATCH v3] Add foo"), "[PATCH v3] Add foo");
        assert_eq!(display("Regression in 19"), "Regression in 19");
    }

    #[test]
    fn keys_ignore_patch_tags_and_case() {
        let original = key("[PATCH] Add SQL/JSON ON MISMATCH clause to JSON_VALUE");
        assert_eq!(original, "add sql/json on mismatch clause to json_value");
        assert_eq!(
            key("Re: [PATCH v3 2/7] Add SQL/JSON ON MISMATCH clause to JSON_VALUE."),
            original
        );
        assert_eq!(
            key("Re: WIP: Add SQL/JSON on mismatch clause to json_value"),
            original
        );
        assert_eq!(
            key("[RFC][POC] Add SQL/JSON ON MISMATCH clause to JSON_VALUE"),
            original
        );
        assert_eq!(key("Re: "), "");
        assert_eq!(key("Re: ?"), "");
        assert_eq!(key("[bug] Crash in REPACK"), "crash in repack");
        assert_eq!(key("Refactor the [v2] parser"), "refactor the [v2] parser");
    }

    #[test]
    fn short_keys_are_not_distinctive() {
        assert!(!is_distinctive(&key("Re: Fix typo")));
        assert!(!is_distinctive(&key("Re: Typos in docs")));
        assert!(is_distinctive(&key(
            "Re: First draft of PG 19 release notes"
        )));
        assert!(!is_distinctive(""));
    }

    #[test]
    fn recognizes_replies() {
        assert!(is_reply("Re: foo"));
        assert!(is_reply("[HACKERS] Re: foo"));
        assert!(is_reply("Fwd: foo"));
        assert!(!is_reply("Regression in foo"));
        assert!(!is_reply("[PATCH] foo"));
    }
}
