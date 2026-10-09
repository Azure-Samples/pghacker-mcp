# pghacker-mcp

pghacker-mcp gives AI agents cited answers about PostgreSQL's design history and source code. It is a [PilotSwarm](https://github.com/microsoft/PilotSwarm) **0.8.2** app with three parts (see [docs/design.md](docs/design.md)):

- **pghacker-mcp**: a public, stateless MCP server in async Rust. It serves the pgsql-hackers corpus and the PostgreSQL source through a read-only database role.
- **pghacker-indexer**: an internal async Rust service. Its workers retrieve sources (monthly mbox archives, seed threads, git), parse, thread, embed and project them into an Apache AGE graph. Its MCP tools serve the crawler sessions.
- **Crawler sessions**: PilotSwarm agents in `plugin/` that extract features, decisions and author positions, deduplicate them, and submit them with verbatim citations.

Wave 1 runs everything locally on test slices of the data:
- two months of pgsql-hackers (June–July 2026);
- whole threads on SQL/PGQ and SQL/JSON;
- a PG19 code subset (`src/backend/parser`, `src/backend/utils/adt`, `src/include/nodes`, `contrib/pg_trgm`).

## Run locally

Requires **Node.js 24+**, npm, **Rust** (the toolchain in `rust-toolchain.toml`), and a local Docker engine with Docker Compose. The scripts target macOS and Linux.

```bash
npm run setup
npm start
```

| Service | Address |
| --- | --- |
| Browser portal | `http://127.0.0.1:3001` |
| pghacker-mcp (MCP) | `http://127.0.0.1:3102/mcp` |
| pghacker-indexer (MCP) | `http://127.0.0.1:3101/mcp` |
| PostgreSQL 17 + AGE, pgvector, pg_trgm, pg_textsearch | `127.0.0.1:55433` |

**Setup** downloads and verifies the PilotSwarm release tarballs, installs dependencies, and builds the two Rust binaries. It also creates an ignored `.env` (owner-only permissions) with random passwords for:
- PilotSwarm's database;
- `pgh_indexer`, which writes schema `pghacker` and the graph `pghacker_graph`;
- `pgh_reader`, which pghacker-mcp uses: read-only, with a 5-second statement timeout.

An existing `.env` is kept, and missing role passwords are added to it.

**`npm start`** runs these steps in order:
1. Starts PostgreSQL. The image in `db/Dockerfile` adds the PGDG builds of AGE 1.7, pgvector 0.8 and pg_textsearch 1.5.
2. Rebuilds the binaries if needed (`cargo build --release --locked`).
3. Bootstraps the roles, extensions, schema and graph, and runs the migrations.
4. Starts and supervises both services, restarting either one if it exits. Logs go to `.runtime/logs`.
5. Starts the PilotSwarm worker and portal.

Each process gets only the credentials it needs: the indexer gets `pgh_indexer`, pghacker-mcp gets `pgh_reader`, and the portal gets only the two MCP URLs.

Everything listens on loopback only, and sign-in is disabled: **don't expose it**. `npm run stop` stops the portal, the services and PostgreSQL; the data volume is kept. `npm run db:reset` (with the app stopped) drops and recreates schema `pghacker` and the graph, and leaves PilotSwarm's data alone.

### Configuration

| Setting (`.env`) | Purpose |
| --- | --- |
| `HORIZON_EMBED_URL`, `HORIZON_EMBED_MODEL`, `HORIZON_EMBED_DIM=1536`, `HORIZON_EMBED_API_KEY` (`_API_KEY_HEADER`, `_BEARER`) | Embeddings for semantic and hybrid search, configured the same way as PilotSwarm's embedder. Example: an Azure OpenAI `text-embedding-3-small` deployment's `/embeddings?api-version=2024-10-21` URL. Without them, search uses full text only. |
| `GITHUB_TOKEN` (or in the shell) | GitHub Copilot models for the agents. `config/model_providers.json` lists them, with `github-copilot:gpt-6-luna` as the default. |
| `PGHACKER_INDEXER_PORT`, `PGHACKER_MCP_PORT` | Service ports (3101, 3102). |

`config/test-slices.toml` chooses the months, seed threads and code paths that wave 1 retrieves.

## Tools

**pghacker-mcp**:

| Area | Tools |
| --- | --- |
| Mail | `search_threads`, `get_thread`, `search_extracts`, `author_perspective`, `search_authors`, `get_author`. The search tools take `mode`: `fulltext` (BM25), `semantic` (embeddings) or `hybrid` (default). |
| SQL and graph | `get_ontology` describes the data. `query` runs one read-only SQL statement, including Cypher on `pghacker_graph`, and returns CSV. |
| Code | `search` is a first-class code search. `grep` runs grep command lines. `read_file` and `list_files` read the loaded source. |

**pghacker-indexer**:

| Area | Tools |
| --- | --- |
| Sources and progress | `add_source`, `remove_source`, `list_sources`, `ingest_status` |
| Extraction | `next_extraction_batch`, `read_messages`, `match_extracts`, `submit_extracts` |
| Repair | `find_duplicates`, `merge_authors`, `merge_extracts`, `undo_merge` |

Results cite Message-IDs, verbatim quotes and file paths. Email addresses are used only to match authors: they live in a table the reader can't access, and bodies are stored with addresses redacted.

## Build the corpus

With the app running:

1. Add the sources. Ask a `pghacker-crawler` session in the portal, or call the indexer's `add_source` with `{kind: mailing_list, name: pgsql-hackers}` and `{kind: postgres_version, name: "19"}`.
2. Watch the progress with `ingest_status`. Workers download the mbox months and seed threads (2 requests/s, cached in `.runtime/cache`), fetch the pinned PG19 commit with gix, embed chunks, resolve file anchors and sync the graph.
3. Extract:

```bash
npm run crawl -- --pilot                          # 10 threads in one pghacker-extract session, for review
npm run crawl -- --concurrency 8                  # then the rest, 8 sessions at a time
npm run crawl -- --dedup                          # a pghacker-dedup session merges duplicates
npm run crawl -- --supervisor                     # or let the pghacker-crawler agent run the pilot
```

The worker runs `PILOTSWARM_WORKER_CONCURRENCY` session turns at once (8 by default; set it in `.env`), so keep `--concurrency` at or below it.

## Checks

```bash
npm test                 # launcher, configuration, readiness and evaluation helpers
npm run rust:check       # rustfmt, clippy -D warnings, and the Rust tests (they create temporary databases)
npm run smoke            # with the app running: both MCP servers, the portal, the worker and the agents
npm run eval             # with the corpus built: the 20-question evaluation (eval/questions.json)
```

The code-search exactness replay lives in `crates/pghacker-replay`. Its PG19 replay compares `search` and `grep` with ripgrep and GNU grep. Its study replay reruns recorded agent searches, which are private: they're read from outside the repository and its output stays under `.runtime/replay`.

## Repository

| Path | Contents |
| --- | --- |
| `crates/pghacker-core` | Config, pools, bootstrap and migrations, embeddings, search modes, MCP plumbing |
| `crates/pghacker-mail` | Archive client, mbox and MIME parsing, threading, authors, patches, anchors, chunks |
| `crates/pghacker-code` | Git snapshot loader (gix), one row per line |
| `crates/pghacker-codesearch` | pg_trgm code search, the grep translator, `read_file`, `list_files` |
| `crates/pghacker-sqlquery` | The read-only `query` tool |
| `crates/pghacker-api` | Mail and graph tools |
| `crates/pghacker-indexer`, `crates/pghacker-mcp` | The two services |
| `crates/pghacker-replay` | The exactness replay harness |
| `migrations/`, `db/` | Schema and the PostgreSQL image |
| `plugin/` | Branding, agents and the MCP catalog |
| `scripts/` | The launcher, crawl and evaluation drivers |
| `vendor/tokio-postgres` | A fork of tokio-postgres 0.7.18 (adds `bind_text`) |
