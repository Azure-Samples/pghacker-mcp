// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! What the feedback form says to the user, all in one place, so that a
//! change shows in review. The client shows it, names the server, and adds
//! its own buttons to decline or cancel. The model never reads it. The tests
//! hold every text to a few rules: no pressure, and nothing that reads as an
//! instruction.

/// The form's message: none. The rating's title asks the question, and the
/// client names the server.
pub const PROMPT: &str = "";

/// The rating's field: its title, and its choices.
pub const RATING_TITLE: &str = "Was the last answer helpful?";
pub const HELPFUL: &str = "👍 Helpful";
pub const NOT_HELPFUL: &str = "👎 Not helpful";
pub const SKIP: &str = "Skip";

/// The comment's field, with comments on.
pub const COMMENT_TITLE: &str = "Anything to add?";
pub const COMMENT_HELP: &str =
    "Optional, up to 500 characters. Please leave out personal or confidential information.";

#[cfg(test)]
mod tests {
    use super::*;

    /// Words that pressure, hide, or read as an instruction.
    const DENIED: &[&str] = &[
        "ignore",
        "disregard",
        "do not tell",
        "don't tell",
        "must",
        "immediately",
        "mandatory",
        "required",
        "secret",
        "urgent",
        "reward",
    ];

    /// `text` in lowercase, its punctuation as spaces, between spaces.
    fn words(text: &str) -> String {
        let spaced: String = text
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '\'' {
                    c
                } else {
                    ' '
                }
            })
            .collect();
        format!(" {spaced} ")
    }

    #[test]
    fn no_text_pressures_or_instructs() {
        for text in [
            PROMPT,
            RATING_TITLE,
            HELPFUL,
            NOT_HELPFUL,
            SKIP,
            COMMENT_TITLE,
            COMMENT_HELP,
        ] {
            let words = words(text);
            for denied in DENIED {
                assert!(
                    !words.contains(&format!(" {denied} ")),
                    "{denied:?} in {text:?}"
                );
            }
        }
    }

    #[test]
    fn the_form_asks_one_question() {
        assert_eq!(PROMPT, "");
        assert_eq!(RATING_TITLE, "Was the last answer helpful?");
        assert_eq!(
            [HELPFUL, NOT_HELPFUL, SKIP],
            ["👍 Helpful", "👎 Not helpful", "Skip"]
        );
    }
}
