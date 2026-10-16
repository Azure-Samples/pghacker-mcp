// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The feedback form, an `elicitation/create` request in an MRTR
//! `input_required` result that answers the call it holds, and what the
//! client's retry of that call says the user chose.

use rmcp::model::{
    ElicitRequest, ElicitRequestParams, ElicitationSchema, InputRequest, InputRequiredResult,
};
use serde_json::{Map, Value, json};

use super::text;

/// The form's key in `inputRequests` and in the retry's `inputResponses`.
pub(super) const KEY: &str = "feedback";
/// A comment's characters at most.
const COMMENT_CHARS: usize = 500;
/// The comment's field. rmcp writes the form through a JSON map, which sorts
/// its keys, and clients show fields in the order they arrive: this name
/// sorts after `rating`, so the comment comes after the rating.
const REMARKS: &str = "remarks";

/// The form, with `state` for the retry to echo: a rating to choose, with
/// no default, and with comments on, an optional comment after it.
pub(super) fn form(comments: bool, state: String) -> InputRequiredResult {
    let mut properties = Map::new();
    properties.insert(
        "rating".into(),
        json!({
            "type": "string",
            "title": text::RATING_TITLE,
            "oneOf": [
                { "const": "up", "title": text::HELPFUL },
                { "const": "down", "title": text::NOT_HELPFUL },
                { "const": "skip", "title": text::SKIP },
            ],
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
    InputRequiredResult::new(
        Some([(KEY.to_owned(), request)].into_iter().collect()),
        Some(state),
    )
}

/// What the user chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Answer {
    /// `rated`, `skipped`, `declined` or `dismissed`.
    pub outcome: &'static str,
    /// `up` or `down`, when rated.
    pub rating: Option<&'static str>,
    /// With comments on, what the user wrote.
    pub comment: Option<String>,
}

/// The user's choice in the client's `response` to the form: 👍 or 👎 is a
/// rating; Skip, a skip; a decline, a decline; a cancel, or a response
/// that's missing or malformed, a dismissal.
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
            let choice = content
                .and_then(|c| c.get("rating"))
                .and_then(Value::as_str);
            let (outcome, rating) = match choice {
                Some("up") => ("rated", Some("up")),
                Some("down") => ("rated", Some("down")),
                Some("skip") => ("skipped", None),
                _ => ("dismissed", None),
            };
            let comment = content
                .filter(|_| comments)
                .and_then(|c| c.get(REMARKS))
                .and_then(Value::as_str)
                .map(clean)
                .filter(|comment| !comment.is_empty());
            Answer {
                outcome,
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
    fn the_form_asks_for_a_rating_with_no_default() {
        let form = serde_json::to_value(form(false, "fb2.state".into())).unwrap();
        assert_eq!(form["resultType"], "input_required");
        assert_eq!(form["requestState"], "fb2.state");
        let request = &form["inputRequests"][KEY];
        assert_eq!(request["method"], "elicitation/create");
        let params = &request["params"];
        assert_eq!(params["mode"], "form");
        assert_eq!(params["message"], "");
        let schema = &params["requestedSchema"];
        assert_eq!(schema["required"], json!(["rating"]));
        assert_eq!(
            schema["properties"],
            json!({ "rating": {
                "type": "string",
                "title": "Was the last answer helpful?",
                "oneOf": [
                    { "const": "up", "title": "👍 Helpful" },
                    { "const": "down", "title": "👎 Not helpful" },
                    { "const": "skip", "title": "Skip" },
                ],
            } })
        );
    }

    #[test]
    fn with_comments_the_form_adds_one_after_the_rating() {
        let form = form(true, "fb2.state".into());
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
        assert_eq!((up.outcome, up.rating), ("rated", Some("up")));
        let down = answer(Some(&accept(json!({ "rating": "down" }))), false);
        assert_eq!((down.outcome, down.rating), ("rated", Some("down")));
        let skipped = answer(Some(&accept(json!({ "rating": "skip" }))), false);
        assert_eq!((skipped.outcome, skipped.rating), ("skipped", None));
        for dismissed in [
            accept(json!({ "rating": "meh" })),
            accept(json!({})),
            json!({ "action": "accept" }),
            json!({ "action": "cancel" }),
            json!({ "action": "bogus" }),
            json!("garbage"),
        ] {
            let answer = answer(Some(&dismissed), false);
            assert_eq!(
                (answer.outcome, answer.rating),
                ("dismissed", None),
                "{dismissed}"
            );
        }
        assert_eq!(answer(None, false).outcome, "dismissed");
        let declined = answer(Some(&json!({ "action": "decline" })), false);
        assert_eq!((declined.outcome, declined.rating), ("declined", None));
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
