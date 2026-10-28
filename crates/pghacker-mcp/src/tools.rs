// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! pghacker-mcp's tools: mail and graph search, read-only SQL, and code search.

use std::sync::{Arc, LazyLock};

use include_dir::{Dir, include_dir};
use rmcp::ErrorData;
use serde::Deserialize;
use serde_json::Value;

use pghacker_api::descriptions as api;
use pghacker_api::{Searcher, extracts, ontology, people, threads};
use pghacker_codesearch as code;
use pghacker_core::calls::CallPool;
use pghacker_core::embed::Embedder;
use pghacker_core::mcp::{Effect, Output, ToolSpec, Tools, failed, parse, schema};
use pghacker_core::skills::Skills;
use pghacker_core::text::strip_presentation_controls;
use pghacker_core::usage::Scope;
use pghacker_sqlquery as sql;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

const READ: Effect = Effect::Read;

/// The skill for reviewing as a given author would, which the instructions name.
#[cfg(test)]
pub const REVIEW_SKILL: &str = "skill://review-as-author/SKILL.md";
/// The tool that loads it, for clients without the skills extension.
#[cfg(test)]
pub const REVIEW_TOOL: &str = "load_skill_review_as_author";

/// The instructions both kinds of client get, before the review sentence.
macro_rules! instructions {
    ($review:literal) => {
        concat!(
            "pghacker-mcp answers questions about PostgreSQL's design history and \
code with citations. Mail: pgsql-hackers threads, authors, and extracted features, decisions and positions \
(search_threads, get_thread, search_extracts, author_perspective, search_authors, get_author), searched by full \
text, semantic search or both; search_patches finds lines in the patches posted, by thread, and the commits that \
added them. Graph and SQL: get_ontology describes the AGE graph pghacker_graph and the tables; query runs \
read-only SQL and Cypher. Code, over every commit of every branch and tag: search (first-class code search), \
grep (grep command lines), read_file and list_files read the version named (a major version, release, branch, \
tag, commit SHA or REL_17_STABLE@2024-06-01; default the newest release branch) and paths below \
/branch/<name>/, /tag/<name>/ or /commit/<rev>/; blame, get_commit and search_commits answer as git blame, \
git show and git log, with the threads that discussed each commit. Cite Message-IDs, commit SHAs and file \
paths with line numbers in answers. To review a patch, commit or design as a given author would (Tom Lane, \
say), ",
            $review,
            " Users can share feedback, or report a problem, at \
https://github.com/Azure-Samples/pghacker-mcp/discussions."
        )
    };
}

/// The server's instructions, for a client without the skills extension,
/// which gets the tool that loads the review skill.
pub const INSTRUCTIONS: &str =
    instructions!("call load_skill_review_as_author and follow what it returns.");

/// The instructions for a client that declares the skills extension, which
/// gets the review skill that way only.
pub const EXTENSION_INSTRUCTIONS: &str =
    instructions!("use the skill review-as-author (skill://review-as-author/SKILL.md).");

/// The server's instructions.
pub fn instructions() -> &'static str {
    INSTRUCTIONS
}

/// The repository's skills folder, embedded at build time.
static SKILLS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../skills");

/// The skills pghacker-mcp serves (the skills extension, SEP-2640).
pub fn skills() -> anyhow::Result<Skills> {
    Skills::from_dir(&SKILLS)
}

pub static TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "search_threads",
        description: api::SEARCH_THREADS,
        effect: READ,
        schema: schema::<threads::SearchThreadsArgs>,
    },
    ToolSpec {
        name: "get_thread",
        description: api::GET_THREAD,
        effect: READ,
        schema: schema::<threads::GetThreadArgs>,
    },
    ToolSpec {
        name: "search_extracts",
        description: api::SEARCH_EXTRACTS,
        effect: READ,
        schema: schema::<extracts::SearchExtractsArgs>,
    },
    ToolSpec {
        name: "author_perspective",
        description: api::AUTHOR_PERSPECTIVE,
        effect: READ,
        schema: schema::<people::AuthorPerspectiveArgs>,
    },
    ToolSpec {
        name: "search_authors",
        description: api::SEARCH_AUTHORS,
        effect: READ,
        schema: schema::<people::SearchAuthorsArgs>,
    },
    ToolSpec {
        name: "get_author",
        description: api::GET_AUTHOR,
        effect: READ,
        schema: schema::<people::GetAuthorArgs>,
    },
    ToolSpec {
        name: "get_ontology",
        description: api::GET_ONTOLOGY,
        effect: READ,
        schema: schema::<NoArgs>,
    },
    ToolSpec {
        name: sql::NAME,
        description: sql::DESCRIPTION,
        effect: READ,
        schema: schema::<sql::QueryArgs>,
    },
    ToolSpec {
        name: "search",
        description: code::SEARCH_DESCRIPTION,
        effect: READ,
        schema: schema::<code::SearchArgs>,
    },
    ToolSpec {
        name: "grep",
        description: code::GREP_DESCRIPTION,
        effect: READ,
        schema: schema::<code::GrepArgs>,
    },
    ToolSpec {
        name: "read_file",
        description: code::READ_FILE_DESCRIPTION,
        effect: READ,
        schema: schema::<code::ReadFileArgs>,
    },
    ToolSpec {
        name: "list_files",
        description: code::LIST_FILES_DESCRIPTION,
        effect: READ,
        schema: schema::<code::ListFilesArgs>,
    },
    ToolSpec {
        name: "search_patches",
        description: code::SEARCH_PATCHES_DESCRIPTION,
        effect: READ,
        schema: schema::<code::SearchPatchesArgs>,
    },
    // git blame, show and log.
    ToolSpec {
        name: "blame",
        description: code::BLAME_DESCRIPTION,
        effect: READ,
        schema: schema::<code::BlameArgs>,
    },
    ToolSpec {
        name: "get_commit",
        description: code::GET_COMMIT_DESCRIPTION,
        effect: READ,
        schema: schema::<code::GetCommitArgs>,
    },
    ToolSpec {
        name: "search_commits",
        description: code::SEARCH_COMMITS_DESCRIPTION,
        effect: READ,
        schema: schema::<code::SearchCommitsArgs>,
    },
];

/// The tools served, with the notes on the commit history that the code
/// tools' and query's descriptions add.
pub fn served() -> &'static [ToolSpec] {
    static SERVED: LazyLock<Vec<ToolSpec>> = LazyLock::new(|| {
        TOOLS
            .iter()
            .map(|tool| ToolSpec {
                name: tool.name,
                description: with_history(tool),
                effect: tool.effect,
                schema: tool.schema,
            })
            .collect()
    });
    &SERVED
}

/// A tool's description, with its note on the commit history.
fn with_history(tool: &ToolSpec) -> &'static str {
    let note = match tool.name {
        "search" | "grep" | "read_file" | "list_files" => code::HISTORY_NOTE,
        sql::NAME => sql::HISTORY_NOTE,
        _ => return tool.description,
    };
    // Built once per tool, for the life of the process.
    Box::leak(format!("{} {note}", tool.description).into_boxed_str())
}

/// The server's state. Cheap to clone.
#[derive(Clone)]
pub struct Server {
    /// The tools' connections, but query's: a call waits a bounded time for
    /// one, and an abandoned call's statements are cancelled.
    pool: CallPool,
    embedder: Option<Embedder>,
    query: Arc<sql::QueryTool>,
}

impl Server {
    pub fn new(pool: CallPool, embedder: Option<Embedder>, query: sql::QueryTool) -> Self {
        Self {
            pool,
            embedder,
            query: Arc::new(query),
        }
    }

    async fn run(&self, name: &str, arguments: Value) -> Result<anyhow::Result<Value>, ErrorData> {
        let embedder = self.embedder.as_ref();
        // The mail searches embed their query before they take a connection,
        // so that no connection waits on the embeddings service.
        match name {
            "search_threads" => {
                let args: threads::SearchThreadsArgs = parse(name, arguments)?;
                return Ok(async {
                    let searcher = Searcher::new(args.mode, &args.query, embedder).await?;
                    let client = self.pool.get().await?;
                    threads::search_threads(&**client, &searcher, args).await
                }
                .await);
            }
            "search_extracts" => {
                let args: extracts::SearchExtractsArgs = parse(name, arguments)?;
                return Ok(async {
                    let searcher = Searcher::new(args.mode, &args.query, embedder).await?;
                    let client = self.pool.get().await?;
                    extracts::search_extracts(&**client, &searcher, args).await
                }
                .await);
            }
            "author_perspective" => {
                let args: people::AuthorPerspectiveArgs = parse(name, arguments)?;
                return Ok(async {
                    let searcher = Searcher::new(args.mode, &args.topic, embedder).await?;
                    let client = self.pool.get().await?;
                    people::author_perspective(&**client, &searcher, args).await
                }
                .await);
            }
            _ => {}
        }
        let client = match self.pool.get().await {
            Ok(client) => client,
            Err(error) => return Ok(Err(error)),
        };
        let client = &**client;
        Ok(match name {
            "get_thread" => threads::get_thread(client, parse(name, arguments)?).await,
            "search_authors" => people::search_authors(client, parse(name, arguments)?).await,
            "get_author" => people::get_author(client, parse(name, arguments)?).await,
            "get_ontology" => ontology::get_ontology(client).await,
            "search" => {
                let args: code::SearchArgs = parse(name, arguments)?;
                match code::resolve_revision(client, "postgres", args.version.as_deref()).await {
                    Ok(revision) => code::search(client, &revision, &args)
                        .await
                        .and_then(|result| Ok(serde_json::to_value(result)?)),
                    Err(error) => Err(error),
                }
            }
            "read_file" => {
                let args: code::ReadFileArgs = parse(name, arguments)?;
                match code::resolve_revision(client, "postgres", args.version.as_deref()).await {
                    Ok(revision) => code::read_file(client, &revision, &args)
                        .await
                        .and_then(|result| Ok(serde_json::to_value(result)?)),
                    Err(error) => Err(error),
                }
            }
            "list_files" => {
                let args: code::ListFilesArgs = parse(name, arguments)?;
                match code::resolve_revision(client, "postgres", args.version.as_deref()).await {
                    Ok(revision) => code::list_files(client, &revision, &args)
                        .await
                        .and_then(|result| Ok(serde_json::to_value(result)?)),
                    Err(error) => Err(error),
                }
            }
            "search_patches" => {
                let args: code::SearchPatchesArgs = parse(name, arguments)?;
                code::search_patches(client, Some(code::history::schema_from_env()), &args)
                    .await
                    .and_then(|result| Ok(serde_json::to_value(result)?))
            }
            "search_commits" => {
                let args: code::SearchCommitsArgs = parse(name, arguments)?;
                code::search_commits(client, code::history::schema_from_env(), "postgres", &args)
                    .await
                    .and_then(|result| Ok(serde_json::to_value(result)?))
            }
            "get_commit" => {
                let args: code::GetCommitArgs = parse(name, arguments)?;
                code::get_commit(client, code::history::schema_from_env(), "postgres", &args)
                    .await
                    .and_then(|result| Ok(serde_json::to_value(result)?))
            }
            "blame" => {
                let args: code::BlameArgs = parse(name, arguments)?;
                match code::resolve_revision(client, "postgres", args.version.as_deref()).await {
                    Ok(revision) => code::blame(client, &revision, &args)
                        .await
                        .and_then(|result| Ok(serde_json::to_value(result)?)),
                    Err(error) => Err(error),
                }
            }
            other => {
                return Err(ErrorData::invalid_params(
                    format!("unknown tool {other}"),
                    None,
                ));
            }
        })
    }
}

impl Tools for Server {
    fn specs(&self) -> &'static [ToolSpec] {
        served()
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Output, ErrorData> {
        self.call_with_usage(name, arguments, None).await
    }

    // query records the statements it checked by their tokens in its call's
    // usage; any tool that fails in PostgreSQL records the error's SQLSTATE.
    async fn call_with_usage(
        &self,
        name: &str,
        arguments: Value,
        scope: Option<Scope>,
    ) -> Result<Output, ErrorData> {
        let result = async {
            match name {
                sql::NAME => Ok(self
                    .query
                    .run_with_usage(parse(name, arguments)?, scope.as_ref())
                    .await),
                "grep" => {
                    let args: code::GrepArgs = parse(name, arguments)?;
                    let client = match self.pool.get().await {
                        Ok(client) => client,
                        Err(error) => return Ok(fail(error, scope.as_ref())),
                    };
                    let result = match code::resolve_revision(
                        &**client,
                        "postgres",
                        args.version.as_deref(),
                    )
                    .await
                    {
                        Ok(revision) => code::grep(&**client, &revision, &args).await,
                        Err(error) => Err(error),
                    };
                    Ok(match result {
                        Ok(text) => Output::Text(text),
                        Err(error) => fail(error, scope.as_ref()),
                    })
                }
                _ => Ok(match self.run(name, arguments).await? {
                    Ok(value) => Output::Json(value),
                    Err(error) => fail(error, scope.as_ref()),
                }),
            }
        }
        .await;
        sanitize_output(result)
    }
}

/// Filter retrieved content before MCP creates its structured and text
/// representations, not the stored corpus or the repository-owned skills.
fn sanitize_output(mut result: Result<Output, ErrorData>) -> Result<Output, ErrorData> {
    match &mut result {
        Ok(Output::Json(value)) => sanitize_json(value),
        Ok(Output::Text(text) | Output::Failed(text)) => strip_presentation_controls(text),
        Err(error) => {
            let mut message = error.message.to_string();
            strip_presentation_controls(&mut message);
            error.message = message.into();
            if let Some(data) = &mut error.data {
                sanitize_json(data);
            }
        }
    }
    result
}

fn sanitize_json(value: &mut Value) {
    match value {
        Value::String(text) => strip_presentation_controls(text),
        Value::Array(values) => values.iter_mut().for_each(sanitize_json),
        Value::Object(object) => object.values_mut().for_each(sanitize_json),
        _ => {}
    }
}

/// [`failed`] with PostgreSQL's SQLSTATE, such as `57014` for the statement
/// timeout, added to `scope` as `errorCode`.
fn fail(error: anyhow::Error, scope: Option<&Scope>) -> Output {
    if let Some(scope) = scope
        && let Some(code) = error.chain().find_map(|cause| {
            cause
                .downcast_ref::<tokio_postgres::Error>()
                .and_then(tokio_postgres::Error::code)
        })
    {
        scope.annotate("errorCode", code.code());
    }
    failed(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outbound_json_filters_nested_strings_without_changing_its_shape() {
        let original = json!({
            "text": "\u{1b}[31m<script>sample</script>\r\n",
            "nested": [{ "filename": "\u{202e}file.patch", "lines": [1, "\u{9b}2J\tcode", null] }],
            "count": 42,
            "score": 0.5,
            "cut": false,
        });
        let expected = json!({
            "text": "[31m<script>sample</script>\n",
            "nested": [{ "filename": "file.patch", "lines": [1, "2J\tcode", null] }],
            "count": 42,
            "score": 0.5,
            "cut": false,
        });
        let Output::Json(value) = sanitize_output(Ok(Output::Json(original.clone()))).unwrap()
        else {
            panic!("a structured result changed its kind");
        };
        assert_eq!(value, expected);
        assert_ne!(original, value);
    }

    #[test]
    fn outbound_text_and_failures_keep_their_kind() {
        let text = "\u{1b}]52;c;c2FtcGxl\u{7}\r\n\u{2066}![image](file:///sample)\u{2069}";
        let expected = "]52;c;c2FtcGxl\n![image](file:///sample)";
        for output in [Output::Text(text.into()), Output::Failed(text.into())] {
            let failed = matches!(output, Output::Failed(_));
            let output = sanitize_output(Ok(output)).unwrap();
            assert_eq!(matches!(output, Output::Failed(_)), failed);
            match output {
                Output::Text(text) | Output::Failed(text) => assert_eq!(text, expected),
                Output::Json(_) => panic!("a text result became structured"),
            }
        }
    }

    #[test]
    fn outbound_rpc_errors_keep_their_code_and_data() {
        let error = ErrorData::invalid_params(
            "bad argument \u{202e}value\u{202c}\u{1b}",
            Some(json!({ "details": ["\u{9d}value", { "line": "a\r\nb" }], "offset": 3 })),
        );
        let code = error.code;
        let Err(error) = sanitize_output(Err(error)) else {
            panic!("a rejected call became successful");
        };
        assert_eq!(error.code, code);
        assert_eq!(error.message, "bad argument value");
        assert_eq!(
            error.data,
            Some(json!({ "details": ["value", { "line": "a\nb" }], "offset": 3 }))
        );
    }

    #[test]
    fn every_tool_is_served() {
        let names: Vec<&str> = served().iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 16);
        for name in ["blame", "get_commit", "search_commits", "search_patches"] {
            assert!(names.contains(&name), "{name}");
        }
    }

    #[test]
    fn the_code_tools_and_query_say_how_to_reach_the_history() {
        for (tool, served) in TOOLS.iter().zip(served()) {
            let described = served.description;
            assert!(described.starts_with(tool.description), "{}", tool.name);
            let note = match tool.name {
                "search" | "grep" | "read_file" | "list_files" => Some(code::HISTORY_NOTE),
                "query" => Some(sql::HISTORY_NOTE),
                _ => None,
            };
            match note {
                Some(note) => assert!(described.ends_with(note), "{}", tool.name),
                None => assert_eq!(described, tool.description, "{}", tool.name),
            }
        }
        for needed in [
            "/branch/<name>/",
            "/tag/<name>/",
            "/commit/<rev>/",
            "REL_17_STABLE@",
        ] {
            assert!(code::HISTORY_NOTE.contains(needed), "{needed}");
            assert!(INSTRUCTIONS.contains(needed), "{needed}");
            assert!(EXTENSION_INSTRUCTIONS.contains(needed), "{needed}");
        }
        for instructions in [INSTRUCTIONS, EXTENSION_INSTRUCTIONS] {
            assert!(
                instructions
                    .ends_with("https://github.com/Azure-Samples/pghacker-mcp/discussions."),
                "{instructions}"
            );
        }
        for tool in TOOLS {
            assert!(INSTRUCTIONS.contains(tool.name), "{}", tool.name);
            assert!(EXTENSION_INSTRUCTIONS.contains(tool.name), "{}", tool.name);
        }
        for table in ["code_commits", "code_commit_threads", "code_line_instances"] {
            assert!(sql::HISTORY_NOTE.contains(table), "{table}");
        }
    }

    #[test]
    fn each_kind_of_client_gets_the_review_skill_its_way() {
        let skills = skills().unwrap();
        let review = skills.get(REVIEW_SKILL).expect("the review skill");
        assert_eq!(review.name, "review-as-author");
        // Hosts get the whole skill even when they only read SKILL.md, and
        // so does its tool.
        assert_eq!(review.files.len(), 1);
        // Without the extension: the tool, and nothing of the extension.
        assert_eq!(review.tool_name(), REVIEW_TOOL);
        assert!(!TOOLS.iter().any(|tool| tool.name == REVIEW_TOOL));
        assert!(INSTRUCTIONS.contains(REVIEW_TOOL));
        for absent in ["skill://", "resources/read", "SKILL.md"] {
            assert!(!INSTRUCTIONS.contains(absent), "{absent}");
        }
        // With it: the skill, and no tool.
        assert!(EXTENSION_INSTRUCTIONS.contains(REVIEW_SKILL));
        assert!(!EXTENSION_INSTRUCTIONS.contains("load_skill"));
        // The rest is the same.
        let lead = |text: &'static str| text.split("(Tom Lane, say), ").next().unwrap();
        assert_eq!(lead(INSTRUCTIONS), lead(EXTENSION_INSTRUCTIONS));
    }

    /// The tool calls the review skill writes as `name(argument: ...)`, with
    /// their arguments' names.
    fn skill_calls(text: &str) -> Vec<(String, Vec<String>)> {
        let span = regex::Regex::new(r"`([a-z_]+)\(([^`]*)\)`").unwrap();
        let argument = regex::Regex::new(r"(?:^|, )([a-z_]+): ").unwrap();
        span.captures_iter(text)
            .map(|call| {
                let names = argument
                    .captures_iter(&call[2])
                    .map(|name| name[1].to_owned())
                    .collect();
                (call[1].to_owned(), names)
            })
            .collect()
    }

    #[test]
    fn the_review_skill_calls_only_served_tools_with_their_arguments() {
        let skills = skills().unwrap();
        let review = skills.get(REVIEW_SKILL).unwrap();
        let calls = skill_calls(&review.files[0].text);
        let called: Vec<&str> = calls.iter().map(|(tool, _)| tool.as_str()).collect();
        for needed in [
            "author_perspective",
            "search_commits",
            "blame",
            "search_threads",
        ] {
            assert!(called.contains(&needed), "the skill should call {needed}");
        }
        for (tool, arguments) in &calls {
            let spec = TOOLS
                .iter()
                .find(|spec| spec.name == tool)
                .unwrap_or_else(|| panic!("the skill calls {tool}, which isn't served"));
            let schema = (spec.schema)();
            let properties = schema["properties"].as_object().unwrap();
            for argument in arguments {
                assert!(
                    properties.contains_key(argument),
                    "{tool} has no argument {argument}"
                );
            }
        }
        let compatibility = review.frontmatter["compatibility"].as_str().unwrap();
        let listed = compatibility
            .split_once(": ")
            .unwrap()
            .1
            .trim_end_matches('.');
        for tool in listed
            .split([',', ' '])
            .filter(|word| !word.is_empty() && *word != "and")
        {
            assert!(TOOLS.iter().any(|spec| spec.name == tool), "{tool}");
        }
    }
}
