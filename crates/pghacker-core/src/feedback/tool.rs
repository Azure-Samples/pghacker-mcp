// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The `user_feedback` tool: its arguments and description, the form its
//! first call answers with (an `elicitation/create` request in an MRTR
//! `input_required` result), and what the client's retry says the user chose.

use rmcp::model::{
    ElicitRequest, ElicitRequestParams, ElicitationSchema, InputRequest, InputRequiredResult, Tool,
    ToolAnnotations,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::text;
use crate::mcp::schema;

/// The tool's name.
pub const TOOL: &str = "user_feedback";
/// The form's key in `inputRequests` and in the retry's `inputResponses`.
pub(super) const KEY: &str = "feedback";
/// A comment's characters at most.
const COMMENT_CHARS: usize = 500;
/// The comment's field. rmcp writes the form through a JSON map, which sorts
/// its keys, and clients show fields in the order they arrive: this name
/// sorts after `rating`, so the comment comes after the rating.
const REMARKS: &str = "remarks";

/// `user_feedback`'s arguments: the id, and nothing else. The rating comes
/// only from the client's form, never from the model.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    /// The feedback_id from a pghacker-mcp result, exactly as given.
    #[schemars(length(max = 200))]
    pub feedback_id: String,
}

/// The tool as tools/list describes it. It records the user's choice in
/// telemetry, so it isn't read-only; a second call with the same id records
/// nothing more.
pub(super) fn spec() -> Tool {
    Tool::new(TOOL, text::DESCRIPTION, schema::<Args>()).with_annotations(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    )
}

/// The form: a rating whose default is Skip, and with comments on, an
/// optional comment after it.
pub(super) fn form(comments: bool) -> InputRequiredResult {
    let mut properties = Map::new();
    properties.insert(
        "rating".into(),
        json!({
            "type": "string",
            "title": text::RATING_TITLE,
            "description": text::RATING_HELP,
            "oneOf": [
                { "const": "up", "title": text::HELPFUL },
                { "const": "down", "title": text::NOT_HELPFUL },
                { "const": "skip", "title": text::SKIP },
            ],
            "default": "skip",
        }),
    );
    if comments {
        properties.insert(
            REMARKS.into(),
            json!({
                "type": "string",
                "title": text::COMMENT_TITLE,
                "description": text::COMMENT_HELP,
                "maxLength": COMMENT_CHARS,
            }),
        );
    }
    let schema: ElicitationSchema = serde_json::from_value(json!({
        "type": "object",
        "properties": properties,
        "required": ["rating"],
    }))
    .expect("the form's schema is an elicitation schema");
    let request = InputRequest::Elicitation(ElicitRequest::new(
        ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: text::PROMPT.to_owned(),
            requested_schema: schema,
        },
    ));
    InputRequiredResult::from_input_requests([(KEY.to_owned(), request)].into_iter().collect())
}

/// What the user chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Answer {
    /// `rated`, `declined` or `dismissed`.
    pub outcome: &'static str,
    /// `up` or `down`, when rated.
    pub rating: Option<&'static str>,
    /// With comments on, what the user wrote.
    pub comment: Option<String>,
}

impl Answer {
    /// What `user_feedback` answers.
    pub(super) fn text(&self) -> &'static str {
        if self.rating.is_some() {
            text::RECORDED
        } else {
            text::NOTHING
        }
    }
}

/// The user's choice in the client's `response` to the form: 👍 or 👎 is a
/// rating; a decline, a decline; Skip, a cancel, or a response that's
/// missing or malformed, a dismissal. Feedback is optional, so nothing asks
/// again.
pub(super) fn answer(response: Option<&Value>, comments: bool) -> Answer {
    let action = response
        .and_then(|r| r.get("action"))
        .and_then(Value::as_str);
    let content = response.and_then(|r| r.get("content"));
    let dismissed = Answer {
        outcome: "dismissed",
        rating: None,
        comment: None,
    };
    match action {
        Some("accept") => {
            let rating = match content
                .and_then(|c| c.get("rating"))
                .and_then(Value::as_str)
            {
                Some("up") => Some("up"),
                Some("down") => Some("down"),
                _ => None,
            };
            let comment = content
                .filter(|_| comments)
                .and_then(|c| c.get(REMARKS))
                .and_then(Value::as_str)
                .map(clean)
                .filter(|comment| !comment.is_empty());
            Answer {
                outcome: if rating.is_some() {
                    "rated"
                } else {
                    "dismissed"
                },
                rating,
                comment,
            }
        }
        Some("decline") => Answer {
            outcome: "declined",
            ..dismissed
        },
        _ => dismissed,
    }
}

/// A comment as kept: control characters as spaces, trimmed, and at most
/// 500 characters.
fn clean(text: &str) -> String {
    let spaced: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    crate::text::truncate_chars(spaced.trim(), COMMENT_CHARS)
        .trim_end()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_takes_only_the_id_and_says_what_it_does() {
        let tool = serde_json::to_value(spec()).unwrap();
        assert_eq!(tool["name"], TOOL);
        let schema = &tool["inputSchema"];
        assert_eq!(schema["required"], json!(["feedback_id"]), "{schema}");
        assert_eq!(schema["additionalProperties"], json!(false), "{schema}");
        assert_eq!(
            schema["properties"]["feedback_id"]["maxLength"],
            json!(super::super::token::MAX_ID),
            "{schema}"
        );
        assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
        assert_eq!(
            tool["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": false,
                    "idempotentHint": true, "openWorldHint": false })
        );
    }

    #[test]
    fn the_form_asks_for_a_rating_whose_default_is_skip() {
        let form = serde_json::to_value(form(false)).unwrap();
        assert_eq!(form["resultType"], "input_required");
        assert!(form.get("requestState").is_none(), "{form}");
        let request = &form["inputRequests"][KEY];
        assert_eq!(request["method"], "elicitation/create");
        let params = &request["params"];
        assert_eq!(params["mode"], "form");
        assert_eq!(params["message"], text::PROMPT);
        let schema = &params["requestedSchema"];
        assert_eq!(schema["required"], json!(["rating"]));
        assert_eq!(
            schema["properties"],
            json!({ "rating": {
                "type": "string",
                "title": text::RATING_TITLE,
                "description": text::RATING_HELP,
                "oneOf": [
                    { "const": "up", "title": "👍 Helpful" },
                    { "const": "down", "title": "👎 Not helpful" },
                    { "const": "skip", "title": "Skip" },
                ],
                "default": "skip",
            } })
        );
    }

    #[test]
    fn with_comments_the_form_adds_one_after_the_rating() {
        let form = form(true);
        let schema = &serde_json::to_value(&form).unwrap()["inputRequests"][KEY]["params"]["requestedSchema"];
        assert_eq!(schema["properties"][REMARKS]["maxLength"], 500);
        assert_eq!(schema["required"], json!(["rating"]));
        // Rating first, as the user reads them.
        let wire = serde_json::to_string(&form).unwrap();
        let rating = wire.find("\"rating\":").unwrap();
        let comment = wire.find(&format!("\"{REMARKS}\":")).unwrap();
        assert!(rating < comment, "{wire}");
    }

    #[test]
    fn the_answer_is_the_users_choice() {
        let accept = |content: Value| json!({ "action": "accept", "content": content });
        let up = answer(Some(&accept(json!({ "rating": "up" }))), false);
        assert_eq!(
            (up.outcome, up.rating, up.text()),
            ("rated", Some("up"), text::RECORDED)
        );
        let down = answer(Some(&accept(json!({ "rating": "down" }))), false);
        assert_eq!((down.outcome, down.rating), ("rated", Some("down")));
        for skipped in [
            accept(json!({ "rating": "skip" })),
            accept(json!({ "rating": "meh" })),
            accept(json!({})),
            json!({ "action": "accept" }),
            json!({ "action": "cancel" }),
            json!({ "action": "bogus" }),
            json!("garbage"),
        ] {
            let dismissed = answer(Some(&skipped), false);
            assert_eq!(
                (dismissed.outcome, dismissed.rating),
                ("dismissed", None),
                "{skipped}"
            );
            assert_eq!(dismissed.text(), text::NOTHING);
        }
        assert_eq!(answer(None, false).outcome, "dismissed");
        let declined = answer(Some(&json!({ "action": "decline" })), false);
        assert_eq!(
            (declined.outcome, declined.text()),
            ("declined", text::NOTHING)
        );
    }

    #[test]
    fn a_comment_is_kept_only_with_comments_on_and_cleaned() {
        let response = json!({ "action": "accept", "content": {
            "rating": "down", "remarks": "  wrong\u{0}thread\n\tcited  " } });
        assert_eq!(answer(Some(&response), false).comment, None);
        let kept = answer(Some(&response), true);
        assert_eq!(kept.comment.as_deref(), Some("wrong thread  cited"));
        let long = json!({ "action": "accept", "content": {
            "rating": "up", "remarks": "é".repeat(600) } });
        assert_eq!(
            answer(Some(&long), true).comment.unwrap().chars().count(),
            COMMENT_CHARS
        );
        let blank = json!({ "action": "accept", "content": { "rating": "up", "remarks": " \n " } });
        assert_eq!(answer(Some(&blank), true).comment, None);
    }
}
