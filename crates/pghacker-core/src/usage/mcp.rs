// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! What the MCP server's events say about a request: the client, a tool's
//! arguments, and its outcome and the shape of its result. A result's
//! content stays out: only counts, numbers, flags, the server's notes and a
//! few names of what was searched, such as the version.
//!
//! An event without details ([`Event::without_details`]) leaves out what
//! users type into tools: of the arguments, it keeps their names and size
//! and the values that are fixed choices of the tool's schema, and of a
//! failure, its type but not its message.

use std::borrow::Cow;
use std::io;
use std::time::Duration;

use rmcp::model::{ClientCapabilities, Implementation, JsonObject};
use serde_json::Value;

use super::event::{Event, MAX_VALUE};
use crate::mcp::Output;
use crate::text::truncate_chars;

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
/// An argument's name's characters at most, without details.
const NAME_CHARS: usize = 64;
/// How deep a schema is followed, through `$ref`s and combinations, to find
/// whether a value is one of its choices.
const MAX_SCHEMA_DEPTH: usize = 8;

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
///
/// Without details, `argumentNames` lists the arguments' names, sorted, as
/// JSON, and `arg.<name>` is recorded only for a value that is one of the
/// fixed choices of the argument's schema in `schema`, the tool's: a boolean
/// or a value of an `enum` or a `const`, or a list of those. A value the
/// schema doesn't allow, like any text, number or object, isn't recorded.
pub(crate) fn arguments(event: &mut Event, arguments: &JsonObject, schema: Option<&JsonObject>) {
    let length = serde_json::to_string(arguments).map_or(0, |json| json.len());
    event.measure("argumentsLength", length as f64);
    if !event.details() {
        choices(event, arguments, schema);
        return;
    }
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

/// [`arguments`] without details: their names, and the values that are
/// choices of `schema`, within the same budget.
fn choices(event: &mut Event, arguments: &JsonObject, schema: Option<&JsonObject>) {
    let mut names: Vec<&str> = arguments
        .keys()
        .map(|name| truncate_chars(name, NAME_CHARS))
        .collect();
    names.sort_unstable();
    names.dedup();
    names.truncate(MAX_ARGUMENTS);
    event.property_cut("argumentNames", &Value::from(names).to_string(), MAX_VALUE);
    let Some(schema) = schema else {
        return;
    };
    let Some(Value::Object(properties)) = schema.get("properties") else {
        return;
    };
    let mut budget = ARGUMENTS_CHARS;
    for (name, value) in arguments {
        let Some(property) = properties.get(name) else {
            continue;
        };
        if !is_choice(schema, property, value, 0) {
            continue;
        }
        let max = budget.min(MAX_VALUE);
        if max == 0 {
            break;
        }
        let text = match value {
            Value::String(text) => Cow::Borrowed(text.as_str()),
            other => Cow::Owned(other.to_string()),
        };
        event.property_cut(format!("arg.{name}"), &text, max);
        budget -= text.chars().count().min(max);
    }
}

/// Whether `value` is one of the fixed choices of `schema`, a schema within
/// `root`: a boolean where it takes one, a value of its `enum` or its
/// `const`, or a list of such values; also through a `$ref` to `root`'s
/// definitions, and `anyOf`, `oneOf` and `allOf`. What it allows then is
/// only ever a boolean or the schema's own constants: never what users type.
fn is_choice(root: &JsonObject, schema: &Value, value: &Value, depth: usize) -> bool {
    let Value::Object(schema) = schema else {
        return false;
    };
    if depth > MAX_SCHEMA_DEPTH {
        return false;
    }
    if let Some(Value::String(reference)) = schema.get("$ref") {
        return definition(root, reference)
            .is_some_and(|target| is_choice(root, target, value, depth + 1));
    }
    if let Some(Value::Array(allowed)) = schema.get("enum") {
        return !value.is_null() && allowed.contains(value);
    }
    if let Some(allowed) = schema.get("const") {
        return !value.is_null() && allowed == value;
    }
    // Each of these combinations lets the value through only if one of its
    // schemas has it among its choices.
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(schemas)) = schema.get(key) {
            return schemas
                .iter()
                .any(|schema| is_choice(root, schema, value, depth + 1));
        }
    }
    let takes = |kind: &str| match schema.get("type") {
        Some(Value::String(type_)) => type_ == kind,
        Some(Value::Array(types)) => types.iter().any(|type_| type_ == kind),
        _ => false,
    };
    match value {
        Value::Bool(_) => takes("boolean"),
        Value::Array(items) if takes("array") => schema.get("items").is_some_and(|schema| {
            items
                .iter()
                .all(|item| is_choice(root, schema, item, depth + 1))
        }),
        _ => false,
    }
}

/// The definition a local `$ref` such as `#/$defs/SearchMode` points to.
fn definition<'a>(root: &'a JsonObject, reference: &str) -> Option<&'a Value> {
    let mut path = reference.strip_prefix("#/")?.split('/');
    let mut target = root.get(unescape(path.next()?).as_ref())?;
    for segment in path {
        target = target.get(unescape(segment).as_ref())?;
    }
    Some(target)
}

/// A JSON pointer's segment, unescaped: `~1` is `/` and `~0` is `~`.
fn unescape(segment: &str) -> Cow<'_, str> {
    if segment.contains('~') {
        Cow::Owned(segment.replace("~1", "/").replace("~0", "~"))
    } else {
        Cow::Borrowed(segment)
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

/// A call that failed: `error_type`, such as `ToolError`, and, with
/// details, its message, which may quote the user.
pub(crate) fn failed(event: &mut Event, error_type: &str, message: &str) {
    event
        .fail()
        .property("activityStatus", "Failed")
        .property("errorType", error_type);
    if event.details() {
        event.property_cut("errorMessage", message, MESSAGE_CHARS);
    }
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
            None,
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
            None,
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

    #[test]
    fn without_details_a_failure_names_only_its_type() {
        let mut event = Event::new("McpToolCall");
        event.without_details();
        failed(
            &mut event,
            "ToolError",
            "ERROR:  column \"secret\" does not exist",
        );
        assert!(event.is_failed());
        assert_eq!(event.get_property("activityStatus"), Some("Failed"));
        assert_eq!(event.get_property("errorType"), Some("ToolError"));
        assert_eq!(event.get_property("errorMessage"), None);
    }

    /// A tool's schema as schemars writes them: enums by `$ref`, as `enum`
    /// or a `oneOf` of `const`s, `Option`s as `anyOf` or `type` arrays with
    /// `null`, and an `allOf` around a `$ref` with its own description.
    fn tool_schema() -> JsonObject {
        object(json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "mode": { "$ref": "#/$defs/SearchMode" },
                "kind": { "anyOf": [{ "$ref": "#/$defs/ExtractKind" }, { "type": "null" }] },
                "kinds": { "type": "array", "items": { "$ref": "#/$defs/ExtractKind" } },
                "order": { "allOf": [{ "$ref": "#/$defs/Order" }], "description": "Newest first?" },
                "diff": { "type": "boolean" },
                "cut": { "type": ["boolean", "null"] },
                "limit": { "type": "integer", "format": "uint32", "minimum": 0 },
                "author": { "anyOf": [{ "type": "integer" }, { "type": "string" }] },
                "paths": { "type": ["array", "null"], "items": { "type": "string" } },
                "since": { "type": "string", "format": "date" },
                "filters": { "type": "object", "properties": { "diff": { "type": "boolean" } } },
                "slash": { "$ref": "#/$defs/a~1b" },
                "remote": { "$ref": "https://example.com/schema.json" },
                "cycle": { "$ref": "#/$defs/Cycle" },
            },
            "$defs": {
                "SearchMode": { "oneOf": [
                    { "const": "hybrid", "description": "Words and meaning." },
                    { "const": "keyword", "description": "Words." },
                ] },
                "ExtractKind": { "type": "string", "enum": ["decision", "feature"] },
                "Order": { "type": "string", "enum": ["newest", "oldest"] },
                "a/b": { "enum": ["c"] },
                "Cycle": { "$ref": "#/$defs/Cycle" },
            },
        }))
    }

    #[test]
    fn without_details_arguments_keep_their_names_and_the_schemas_choices() {
        let schema = tool_schema();
        let mut event = Event::new("McpToolCall");
        event.without_details();
        let passed = object(json!({
            "query": "SELECT * FROM my_secret",
            "mode": "hybrid",
            "kind": "decision",
            "kinds": ["decision", "feature"],
            "order": "oldest",
            "diff": true,
            "cut": false,
            "limit": 5,
            "author": "Tom Lane",
            "paths": ["src/backend/my_secret"],
            "since": "2024-01-01",
            "filters": { "diff": true },
            "slash": "c",
            "unknown": "my secret too",
        }));
        arguments(&mut event, &passed, Some(&schema));

        assert_eq!(
            event.get_property("argumentNames"),
            Some(
                r#"["author","cut","diff","filters","kind","kinds","limit","mode","order","paths","query","since","slash","unknown"]"#
            )
        );
        let length = serde_json::to_string(&passed).unwrap().len() as f64;
        assert_eq!(event.get_measurement("argumentsLength"), Some(length));
        assert_eq!(event.get_property("arg.mode"), Some("hybrid"));
        assert_eq!(event.get_property("arg.kind"), Some("decision"));
        assert_eq!(
            event.get_property("arg.kinds"),
            Some(r#"["decision","feature"]"#)
        );
        assert_eq!(event.get_property("arg.order"), Some("oldest"));
        assert_eq!(event.get_property("arg.diff"), Some("true"));
        assert_eq!(event.get_property("arg.cut"), Some("false"));
        assert_eq!(event.get_property("arg.slash"), Some("c"));
        for name in [
            "query", "limit", "author", "paths", "since", "filters", "unknown",
        ] {
            assert_eq!(event.get_property(&format!("arg.{name}")), None, "{name}");
        }
        assert_eq!(event.get_property("argumentsTruncated"), None);
    }

    #[test]
    fn without_details_a_value_the_schema_doesnt_offer_isnt_recorded() {
        let schema = tool_schema();
        let mut event = Event::new("McpToolCall");
        event.without_details();
        arguments(
            &mut event,
            &object(json!({
                "mode": "SELECT * FROM my_secret",
                "kind": { "const": "decision" },
                "kinds": ["decision", "my secret"],
                "order": null,
                "diff": "my secret",
                "cut": null,
                "slash": "d",
                "remote": "my secret",
                "cycle": "my secret",
            })),
            Some(&schema),
        );
        for name in [
            "mode", "kind", "kinds", "order", "diff", "cut", "slash", "remote", "cycle",
        ] {
            assert_eq!(event.get_property(&format!("arg.{name}")), None, "{name}");
        }
        assert!(event.get_property("argumentNames").is_some());
    }

    #[test]
    fn without_details_choices_keep_within_the_arguments_budget() {
        let lists = json!({ "type": "array", "items": { "enum": ["x"] } });
        let schema = object(json!({ "properties": {
            "a": lists, "b": lists, "c": lists, "d": lists, "e": lists } }));
        let many = vec!["x"; 3000];
        let mut event = Event::new("McpToolCall");
        event.without_details();
        arguments(
            &mut event,
            &object(json!({ "a": many, "b": many, "c": many, "d": many, "e": many })),
            Some(&schema),
        );
        assert_eq!(event.get_property("arg.a").unwrap().len(), MAX_VALUE);
        assert_eq!(event.get_property("arg.d").unwrap().len(), MAX_VALUE);
        assert_eq!(event.get_property("arg.e"), None);
        assert_eq!(event.get_property("argumentsTruncated"), None);
    }

    #[test]
    fn without_details_an_unknown_tools_arguments_keep_only_their_names() {
        let mut event = Event::new("McpToolCall");
        event.without_details();
        let long = "n".repeat(100);
        arguments(
            &mut event,
            &object(json!({ "diff": true, long.clone(): 1, format!("{long}x"): 2 })),
            None,
        );
        // Names are cut to 64 characters, and listed once.
        assert_eq!(
            event.get_property("argumentNames"),
            Some(format!(r#"["diff","{}"]"#, "n".repeat(NAME_CHARS)).as_str())
        );
        assert_eq!(event.get_property("arg.diff"), None);
    }
}
