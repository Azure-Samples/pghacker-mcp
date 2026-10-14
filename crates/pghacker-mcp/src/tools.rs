//! pghacker-mcp's tools: mail and graph search, read-only SQL, and code search.

use std::sync::{Arc, LazyLock};

use rmcp::ErrorData;
use serde::Deserialize;
use serde_json::Value;

use pghacker_api::descriptions as api;
use pghacker_api::{extracts, ontology, people, threads};
use pghacker_codesearch as code;
use pghacker_core::db::Pool;
use pghacker_core::embed::Embedder;
use pghacker_core::mcp::{Effect, Output, ToolSpec, Tools, failed, parse, schema};
use pghacker_sqlquery as sql;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

const READ: Effect = Effect::Read;

pub const INSTRUCTIONS: &str = "pghacker-mcp answers questions about PostgreSQL's design history and code with \
citations. Mail: pgsql-hackers threads, authors, and extracted features, decisions and positions \
(search_threads, get_thread, search_extracts, author_perspective, search_authors, get_author), searched by full text, \
semantic search or both; search_patches finds lines in the patches posted, by thread. Graph and SQL: get_ontology describes the AGE graph pghacker_graph and the tables; query runs \
read-only SQL and Cypher. Code: search (first-class code search), grep (grep command lines), read_file and \
list_files over the PostgreSQL source loaded (see list_files or get_ontology). Cite Message-IDs and file paths \
with line numbers in answers.";

/// The instructions when the code tools read the commit history.
pub const HISTORY_INSTRUCTIONS: &str = "pghacker-mcp answers questions about PostgreSQL's design history and \
code with citations. Mail: pgsql-hackers threads, authors, and extracted features, decisions and positions \
(search_threads, get_thread, search_extracts, author_perspective, search_authors, get_author), searched by full \
text, semantic search or both; search_patches finds lines in the patches posted, by thread, and the commits that \
added them. Graph and SQL: get_ontology describes the AGE graph pghacker_graph and the tables; query runs \
read-only SQL and Cypher. Code, over every commit of every branch and tag: search (first-class code search), \
grep (grep command lines), read_file and list_files read the version named (a major version, release, branch, \
tag, commit SHA or REL_17_STABLE@2024-06-01; default the newest release branch) and paths below \
/branch/<name>/, /tag/<name>/ or /commit/<rev>/; blame, get_commit and search_commits answer as git blame, \
git show and git log, with the threads that discussed each commit. Cite Message-IDs, commit SHAs and file \
paths with line numbers in answers.";

/// The server's instructions: the history's when the code tools read it.
pub fn instructions() -> &'static str {
    if code::history::schema_from_env().is_some() {
        HISTORY_INSTRUCTIONS
    } else {
        INSTRUCTIONS
    }
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
    // Tools that read the commit history, last: served only with it.
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

/// How many of TOOLS, at the end, need the commit history.
const HISTORY_TOOLS: usize = 3;

/// The tools served: the history's only with PGHACKER_CODE_HISTORY, when
/// the code tools' and query's descriptions also say how to reach it.
pub fn served() -> &'static [ToolSpec] {
    static WITH_HISTORY: LazyLock<Vec<ToolSpec>> = LazyLock::new(|| {
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
    if code::history::schema_from_env().is_some() {
        &WITH_HISTORY
    } else {
        &TOOLS[..TOOLS.len() - HISTORY_TOOLS]
    }
}

/// A tool's description when the code tools read the commit history.
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
                match code::resolve_snapshot(client, "postgres", args.version.as_deref()).await {
                    Ok(snapshot) => code::search(client, &snapshot, &args)
                        .await
                        .and_then(|result| Ok(serde_json::to_value(result)?)),
                    Err(error) => Err(error),
                }
            }
            "read_file" => {
                let args: code::ReadFileArgs = parse(name, arguments)?;
                match code::resolve_snapshot(client, "postgres", args.version.as_deref()).await {
                    Ok(snapshot) => code::read_file(client, &snapshot, &args)
                        .await
                        .and_then(|result| Ok(serde_json::to_value(result)?)),
                    Err(error) => Err(error),
                }
            }
            "list_files" => {
                let args: code::ListFilesArgs = parse(name, arguments)?;
                match code::resolve_snapshot(client, "postgres", args.version.as_deref()).await {
                    Ok(snapshot) => code::list_files(client, &snapshot, &args)
                        .await
                        .and_then(|result| Ok(serde_json::to_value(result)?)),
                    Err(error) => Err(error),
                }
            }
            "search_patches" => {
                let args: code::SearchPatchesArgs = parse(name, arguments)?;
                code::search_patches(client, code::history::schema_from_env(), &args)
                    .await
                    .and_then(|result| Ok(serde_json::to_value(result)?))
            }
            "search_commits" if served().iter().any(|t| t.name == name) => {
                let args: code::SearchCommitsArgs = parse(name, arguments)?;
                match code::history::schema_from_env() {
                    Some(schema) => code::search_commits(client, schema, "postgres", &args)
                        .await
                        .and_then(|result| Ok(serde_json::to_value(result)?)),
                    None => Err(anyhow::anyhow!("search_commits needs the commit history")),
                }
            }
            "get_commit" if served().iter().any(|t| t.name == name) => {
                let args: code::GetCommitArgs = parse(name, arguments)?;
                match code::history::schema_from_env() {
                    Some(schema) => code::get_commit(client, schema, "postgres", &args)
                        .await
                        .and_then(|result| Ok(serde_json::to_value(result)?)),
                    None => Err(anyhow::anyhow!("get_commit needs the commit history")),
                }
            }
            "blame" if served().iter().any(|t| t.name == name) => {
                let args: code::BlameArgs = parse(name, arguments)?;
                match code::resolve_snapshot(client, "postgres", args.version.as_deref()).await {
                    Ok(snapshot) => code::blame(client, &snapshot, &args)
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
                    match code::resolve_snapshot(&**client, "postgres", args.version.as_deref())
                        .await
                    {
                        Ok(snapshot) => code::grep(&**client, &snapshot, &args).await,
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
    fn history_tools_come_last() {
        let names: Vec<&str> = TOOLS[TOOLS.len() - HISTORY_TOOLS..]
            .iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["blame", "get_commit", "search_commits"]);
    }

    #[test]
    fn with_the_history_the_code_tools_and_query_say_how_to_reach_it() {
        for tool in TOOLS {
            let described = with_history(tool);
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
            assert!(HISTORY_INSTRUCTIONS.contains(needed), "{needed}");
        }
        for tool in &TOOLS[TOOLS.len() - HISTORY_TOOLS..] {
            assert!(HISTORY_INSTRUCTIONS.contains(tool.name), "{}", tool.name);
            assert!(!INSTRUCTIONS.contains(tool.name), "{}", tool.name);
        }
        for table in ["code_commits", "code_commit_threads", "code_line_instances"] {
            assert!(sql::HISTORY_NOTE.contains(table), "{table}");
        }
    }
}
