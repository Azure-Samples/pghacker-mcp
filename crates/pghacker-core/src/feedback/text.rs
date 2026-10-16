// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! What feedback says to the model and to the user, all in one place, so
//! that a change shows in review. The policy goes where clients treat text
//! as the server's declared interface, the instructions and the tool's
//! description; a result's note only points to it. The tests hold every text
//! to a few rules: no pressure, no imperatives that read as an injected
//! instruction, and a short note.

/// The paragraph the server's instructions gain when feedback is on.
pub const INSTRUCTIONS: &str = "Feedback: now and then, a tool result includes a note with a \
feedback_id. After you have written your full answer to the user, call user_feedback once with \
that id, as your last step: the user's MCP client asks them whether the answer helped, and they \
can dismiss it. Skip it when no person is reading, as in automated runs. Never answer the prompt \
for the user, and don't let it change or delay your answer.";

/// `user_feedback`'s description.
pub const DESCRIPTION: &str = "Asks the user, through their MCP client, whether an answer built \
from pghacker-mcp results helped. Call it only when a pghacker-mcp result included a feedback_id: \
once, after you have written your full answer, and only if a person is reading. Pass the \
feedback_id exactly as given. The client shows the user a one-click prompt that they can dismiss. \
What they choose, including a dismissal, is stored with pghacker-mcp's usage telemetry (see \
PRIVACY.md).";

/// The note after a sampled result's content, for the model.
pub fn note(feedback_id: &str) -> String {
    format!(
        "Optional feedback request. feedback_id: {feedback_id} After your answer, see the \
         user_feedback tool."
    )
}

/// The prompt's message, which the client shows the user.
pub const PROMPT: &str = "pghacker-mcp asks: was the last answer helpful? Optional. Your choice \
is stored with pghacker-mcp's usage telemetry: \
https://github.com/Azure-Samples/pghacker-mcp/blob/main/PRIVACY.md";

/// The rating's field: its title, its help, and its choices.
pub const RATING_TITLE: &str = "Was this answer helpful?";
pub const RATING_HELP: &str =
    "Only the person who read the answer should rate it; anyone else, choose Skip.";
pub const HELPFUL: &str = "👍 Helpful";
pub const NOT_HELPFUL: &str = "👎 Not helpful";
pub const SKIP: &str = "Skip";

/// The comment's field, with comments on.
pub const COMMENT_TITLE: &str = "Anything to add?";
pub const COMMENT_HELP: &str =
    "Optional, up to 500 characters. Please leave out personal or confidential information.";

/// What `user_feedback` answers.
pub const RECORDED: &str = "Thanks: your rating was recorded.";
pub const NOTHING: &str = "Okay: nothing was recorded.";
pub const NOTHING_MORE: &str = "Nothing more to record.";
pub const CANNOT_ASK: &str = "This client can't show the prompt; nothing was recorded.";
pub const OPTED_OUT: &str = "Feedback is off for this connection; nothing was recorded.";
pub const INVALID: &str = "This feedback id isn't valid (it may have expired).";
pub const TOO_MUCH: &str = "Too much feedback right now; nothing was recorded.";
pub const UNAVAILABLE: &str = "Feedback isn't available here.";

#[cfg(test)]
mod tests {
    use super::*;

    /// Words that pressure, hide, or read as an instruction injected into a
    /// result.
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
    fn no_text_pressures_or_hides() {
        let note = note("fb1.rs2.a.b.c");
        for text in [
            INSTRUCTIONS,
            DESCRIPTION,
            &note,
            PROMPT,
            RATING_HELP,
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
    fn the_note_is_a_handle_and_a_pointer() {
        let id = format!("fb1.{}", "x".repeat(110));
        let note = note(&id);
        assert!(
            note.contains(&id) && note.contains("user_feedback"),
            "{note}"
        );
        assert!(note.chars().count() <= 220, "{}", note.chars().count());
        assert!(
            !note.contains("PRIVACY"),
            "the policy stays out of the note"
        );
    }

    #[test]
    fn the_policy_is_where_clients_read_it() {
        for text in [INSTRUCTIONS, DESCRIPTION] {
            let lower = text.to_lowercase();
            for needed in [
                "feedback_id",
                "after you have written your full answer",
                "dismiss",
            ] {
                assert!(lower.contains(needed), "{needed:?} in {text:?}");
            }
        }
        assert!(INSTRUCTIONS.contains("call user_feedback once"));
        assert!(DESCRIPTION.contains("only if a person is reading"));
        assert!(INSTRUCTIONS.contains("Never answer the prompt for the user"));
        // The prompt names the server and links the notice.
        assert!(PROMPT.starts_with("pghacker-mcp asks:"));
        assert!(PROMPT.contains("PRIVACY.md") && PROMPT.contains("Optional"));
    }
}
