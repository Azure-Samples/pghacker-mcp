// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! What the MCP server's events say about a request: the client, a tool's
//! arguments, and its outcome and the shape of its result. A result's
//! content stays out: only counts, numbers, flags, the server's notes and a
//! few names of what was searched, such as the version.

use std::borrow::Cow;
use std::io;
use std::time::Duration;

use rmcp::model::{ClientCapabilities, Implementation, JsonObject};
use serde_json::Value;

use super::event::{Event, MAX_VALUE};
use crate::mcp::Output;

/// The arguments' characters together at most, within the 64 KB that
/// Application Insights takes in an event.
const ARGUMENTS_CHARS: usize = 32 * 1024;
/// Arguments recorded at most.
const MAX_ARGUMENTS: usize = 32;
/// An error message's characters, or the notes', at most.
const MESSAGE_CHARS: usize = 1024;
/// A result's fields recorded at most.
const MAX_RESULT_FIELDS: usize = 40;
/// A result's string fields that name what was searched, not what was found.
const RESULT_NAMES: &[&str] = &["mode", "repo", "version", "ref", "sha"];

/// The client: its name and version, its protocol version and capabilities.
pub(crate) fn client(
    event: &mut Event,
    info: Option<&Implementation>,
    protocol: Option<&str>,
    capabilities: Option<&ClientCapabilities>,
) {
    if let Some(info) = info {
        event.property_cut("clientName", &info.name, 256);
        event.property_cut("clientVersion", &info.version, 128);
        if let Some(title) = &info.title {
            event.property_cut("clientTitle", title, 256);
        }
    }
    if let Some(protocol) = protocol {
        event.property_cut("protocolVersion", protocol, 64);
    }
    if let Some(json) =
        capabilities.and_then(|capabilities| serde_json::to_string(capabilities).ok())
    {
        event.property_cut("clientCapabilities", &json, MESSAGE_CHARS);
    }
}

/// A tool's arguments, each as `arg.<name>`: a string as it is, anything
/// else as JSON, cut to fit. `argumentsLength` is the arguments' length as
/// JSON, and `argumentsTruncated` says some were cut or left out.
pub(crate) fn arguments(event: &mut Event, arguments: &JsonObject) {
    let length = serde_json::to_string(arguments).map_or(0, |json| json.len());
    event.measure("argumentsLength", length as f64);
    let mut budget = ARGUMENTS_CHARS;
    let mut cut = arguments.len() > MAX_ARGUMENTS;
    for (name, value) in arguments.iter().take(MAX_ARGUMENTS) {
        let text = match value {
            Value::String(text) => Cow::Borrowed(text.as_str()),
            other => Cow::Owned(other.to_string()),
        };
        let max = budget.min(MAX_VALUE);
        if max == 0 {
            cut = true;
            break;
        }
        cut |= event.property_cut(format!("arg.{name}"), &text, max);
        budget -= text.chars().count().min(max);
    }
    if cut {
        event.property("argumentsTruncated", "true");
    }
}

/// How long the call took.
pub(crate) fn duration(event: &mut Event, elapsed: Duration) {
    event.measure("durationMs", elapsed.as_secs_f64() * 1000.0);
}

/// A call that succeeded, and its result's shape.
pub(crate) fn succeeded(event: &mut Event, output: &Output) {
    event.property("activityStatus", "Succeeded");
    shape(event, output);
}

/// A call answered with a request for the user's input (MCP's multi
/// round-trip requests): the client asks the user and calls again.
pub(crate) fn input_required(event: &mut Event) {
    event.property("activityStatus", "InputRequired");
}

/// A call that failed: `error_type`, such as `ToolError`, and its message.
pub(crate) fn failed(event: &mut Event, error_type: &str, message: &str) {
    event
        .fail()
        .property("activityStatus", "Failed")
        .property("errorType", error_type);
    event.property_cut("errorMessage", message, MESSAGE_CHARS);
}

/// The shape of a result: its kind and size; for JSON, each field's count of
/// items or number, flags, the server's notes, and the names in
/// [`RESULT_NAMES`]; for text, its lines.
pub(crate) fn shape(event: &mut Event, output: &Output) {
    match output {
        Output::Json(value) => {
            event
                .property("resultKind", "json")
                .measure("responseBytes", json_len(value) as f64);
            let Value::Object(fields) = value else {
                return;
            };
            for (key, field) in fields.iter().take(MAX_RESULT_FIELDS) {
                let name = format!("result.{key}");
                match field {
                    Value::Array(_) if key == "notes" => {
                        event.property_cut(name, &field.to_string(), MESSAGE_CHARS);
                    }
                    Value::Array(items) => {
                        event.measure(name, items.len() as f64);
                    }
                    Value::Number(number) => {
                        if let Some(number) = number.as_f64() {
                            event.measure(name, number);
                        }
                    }
                    Value::Bool(flag) => {
                        event.property(name, if *flag { "true" } else { "false" });
                    }
                    Value::String(text) if RESULT_NAMES.contains(&key.as_str()) => {
                        event.property_cut(name, text, 256);
                    }
                    _ => {}
                }
            }
        }
        Output::Text(text) => {
            event
                .property("resultKind", "text")
                .measure("responseBytes", text.len() as f64)
                .measure("result.lines", text.lines().count() as f64);
        }
        Output::Failed(text) => {
            event
                .property("resultKind", "error")
                .measure("responseBytes", text.len() as f64);
        }
    }
}

/// `value`'s length as JSON, without writing it out.
fn json_len(value: &Value) -> usize {
    struct Count(usize);
    impl io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> JsonObject {
        match value {
            Value::Object(object) => object,
            _ => unreachable!(),
        }
    }

    #[test]
    fn arguments_are_kept_whole_until_they_dont_fit() {
        let mut event = Event::new("McpToolCall");
        arguments(
            &mut event,
            &object(
                json!({ "query": "logical replication", "limit": 5, "filters": { "since": "2024" } }),
            ),
        );
        assert_eq!(event.get_property("arg.query"), Some("logical replication"));
        assert_eq!(event.get_property("arg.limit"), Some("5"));
        assert_eq!(
            event.get_property("arg.filters"),
            Some(r#"{"since":"2024"}"#)
        );
        assert_eq!(event.get_property("argumentsTruncated"), None);
        assert_eq!(event.get_measurement("argumentsLength"), Some(68.0));

        let mut event = Event::new("McpToolCall");
        let sql = format!("SELECT '{}'", "x".repeat(20_000));
        arguments(
            &mut event,
            &object(json!({ "a": sql, "b": sql, "c": sql, "d": sql, "e": sql })),
        );
        assert_eq!(event.get_property("arg.a").unwrap().len(), MAX_VALUE);
        assert_eq!(event.get_property("arg.d").unwrap().len(), MAX_VALUE);
        // The budget runs out: the last is left out.
        assert_eq!(event.get_property("arg.e"), None);
        assert_eq!(event.get_property("argumentsTruncated"), Some("true"));
    }

    #[test]
    fn a_results_shape_leaves_its_content_out() {
        let mut event = Event::new("McpToolCall");
        succeeded(
            &mut event,
            &Output::Json(json!({
                "repo": "postgres", "version": "17", "ref": "REL_17_STABLE", "sha": "abc123",
                "mode": "hybrid", "total_lines": 42, "returned": 10, "cut": true,
                "complete": false, "lines": [1, 2, 3], "notes": ["semantic search is off"],
                "content": "the file's text", "author": { "name": "x" },
            })),
        );
        assert_eq!(event.get_property("activityStatus"), Some("Succeeded"));
        assert_eq!(event.get_property("resultKind"), Some("json"));
        assert_eq!(event.get_property("result.version"), Some("17"));
        assert_eq!(event.get_property("result.mode"), Some("hybrid"));
        assert_eq!(event.get_property("result.cut"), Some("true"));
        assert_eq!(event.get_property("result.complete"), Some("false"));
        assert_eq!(
            event.get_property("result.notes"),
            Some(r#"["semantic search is off"]"#)
        );
        assert_eq!(event.get_measurement("result.total_lines"), Some(42.0));
        assert_eq!(event.get_measurement("result.lines"), Some(3.0));
        assert_eq!(event.get_property("result.content"), None);
        assert_eq!(event.get_property("result.author"), None);
        assert!(event.get_measurement("responseBytes").unwrap() > 100.0);

        let mut event = Event::new("McpToolCall");
        succeeded(&mut event, &Output::Text("a,b\n1,2\n".into()));
        assert_eq!(event.get_measurement("result.lines"), Some(2.0));
        assert_eq!(event.get_measurement("responseBytes"), Some(8.0));
    }

    #[test]
    fn a_failure_names_its_type_and_message() {
        let mut event = Event::new("McpToolCall");
        failed(&mut event, "ToolError", &"e".repeat(5000));
        assert!(event.is_failed());
        assert_eq!(event.get_property("activityStatus"), Some("Failed"));
        assert_eq!(event.get_property("errorType"), Some("ToolError"));
        assert_eq!(
            event.get_property("errorMessage").unwrap().len(),
            MESSAGE_CHARS
        );
    }
}
