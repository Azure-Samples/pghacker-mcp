// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! pghacker-mcp's tools: mail and graph search, read-only SQL, and code search.

use std::sync::{Arc, LazyLock};

use include_dir::{Dir, include_dir};
use rmcp::ErrorData;
use serde::Deserialize;
use serde_json::Value;

use pghacker_api::descriptions as api;
use pghacker_api::{extracts, ontology, people, threads};
use pghacker_codesearch as code;
use pghacker_core::db::Pool;
use pghacker_core::embed::Embedder;
use pghacker_core::mcp::{Effect, Output, ToolSpec, Tools, failed, parse, schema};
use pghacker_core::skills::Skills;
use pghacker_sqlquery as sql;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

const READ: Effect = Effect::Read;

/// The skill for reviewing as a given author would, which the instructions name.
#[cfg(test)]
pub const REVIEW_SKILL: &str = "skill://review-as-author/SKILL.md";

/// The server's instructions.
pub const INSTRUCTIONS: &str = "pghacker-mcp answers questions about PostgreSQL's design history and \
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
say), follow the skill skill://review-as-author/SKILL.md (resources/read reads it). In short: their positions \
with author_perspective (topic: what the change does; paths: its files), their commits in those files with \
search_commits (author, paths) and blame, and each point attributed to them only with a quote of their own \
message.";

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
    pool: Pool,
    embedder: Option<Embedder>,
    query: Arc<sql::QueryTool>,
}

impl Server {
    pub fn new(pool: Pool, embedder: Option<Embedder>, query: sql::QueryTool) -> Self {
        Self {
            pool,
            embedder,
            query: Arc::new(query),
        }
    }

    async fn run(&self, name: &str, arguments: Value) -> Result<anyhow::Result<Value>, ErrorData> {
        let client = match self.pool.get().await {
            Ok(client) => client,
            Err(error) => return Ok(Err(anyhow::anyhow!("the database is unavailable: {error}"))),
        };
        let client = &**client;
        let embedder = self.embedder.as_ref();
        Ok(match name {
            "search_threads" => {
                threads::search_threads(client, embedder, parse(name, arguments)?).await
            }
            "get_thread" => threads::get_thread(client, parse(name, arguments)?).await,
            "search_extracts" => {
                extracts::search_extracts(client, embedder, parse(name, arguments)?).await
            }
            "author_perspective" => {
                people::author_perspective(client, embedder, parse(name, arguments)?).await
            }
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
        match name {
            "query" => Ok(self.query.run(parse(name, arguments)?).await),
            "grep" => {
                let args: code::GrepArgs = parse(name, arguments)?;
                let client = match self.pool.get().await {
                    Ok(client) => client,
                    Err(error) => {
                        return Ok(Output::Failed(format!(
                            "the database is unavailable: {error}"
                        )));
                    }
                };
                let result =
                    match code::resolve_revision(&**client, "postgres", args.version.as_deref())
                        .await
                    {
                        Ok(revision) => code::grep(&**client, &revision, &args).await,
                        Err(error) => Err(error),
                    };
                Ok(match result {
                    Ok(text) => Output::Text(text),
                    Err(error) => failed(error),
                })
            }
            _ => Ok(match self.run(name, arguments).await? {
                Ok(value) => Output::Json(value),
                Err(error) => failed(error),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
        for tool in TOOLS {
            assert!(INSTRUCTIONS.contains(tool.name), "{}", tool.name);
        }
        for table in ["code_commits", "code_commit_threads", "code_line_instances"] {
            assert!(sql::HISTORY_NOTE.contains(table), "{table}");
        }
    }

    #[test]
    fn the_skills_load_and_the_instructions_name_the_review_skill() {
        let skills = skills().unwrap();
        let review = skills.get(REVIEW_SKILL).expect("the review skill");
        assert_eq!(review.name, "review-as-author");
        assert!(INSTRUCTIONS.contains(REVIEW_SKILL));
        // Hosts get the whole skill even when they only read SKILL.md.
        assert_eq!(review.files.len(), 1);
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
