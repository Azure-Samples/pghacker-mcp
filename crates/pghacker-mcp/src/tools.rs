//! pghacker-mcp's tools: mail and graph search, read-only SQL, and code search.

use std::sync::Arc;

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
semantic search or both. Graph and SQL: get_ontology describes the AGE graph pghacker_graph and the tables; query runs \
read-only SQL and Cypher. Code: search (first-class code search), grep (grep command lines), read_file and \
list_files over the PostgreSQL source loaded (see list_files or get_ontology). Cite Message-IDs and file paths \
with line numbers in answers.";

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

/// The tools served: the history's only with PGHACKER_CODE_HISTORY.
pub fn served() -> &'static [ToolSpec] {
    if code::history::schema_from_env().is_some() {
        TOOLS
    } else {
        &TOOLS[..TOOLS.len() - HISTORY_TOOLS]
    }
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
}
