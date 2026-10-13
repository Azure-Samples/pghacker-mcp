# pghacker-mcp

pghacker-mcp gives AI agents cited answers about PostgreSQL's design history and source code. It is a [PilotSwarm](https://github.com/microsoft/PilotSwarm) **0.8.2** app with three parts (see [docs/design.md](docs/design.md)):

- **pghacker-mcp**: a public, stateless MCP server in async Rust. It serves the pgsql-hackers corpus and the PostgreSQL source through a read-only database role.
- **pghacker-indexer**: an internal async Rust service. Its workers retrieve sources (monthly mbox archives, seed threads, git), parse, thread, embed and project them into an Apache AGE graph. Its MCP tools serve the crawler sessions.
- **Crawler sessions**: PilotSwarm agents in `plugin/` that extract features, decisions and author positions, deduplicate them, and submit them with verbatim citations.

Locally, it runs on test slices of the data:
- two months of pgsql-hackers (June–July 2026);
- whole threads on SQL/PGQ and SQL/JSON;
- a PG19 code subset (`src/backend/parser`, `src/backend/utils/adt`, `src/include/nodes`, `contrib/pg_trgm`).

In Azure ([deploy/README.md](deploy/README.md)), it indexes whatever its users ask for, starting from the local corpus.

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

### Graph index readiness

Bootstrap indexes every newly created concrete AGE edge label by `id`, including
empty and future registered labels. The graph mapping migration adds outgoing
`(start_key, label, end_key)` and incoming `(end_key, label, start_key)` B-trees on
`pghacker.graph_edges`. Existing AGE endpoint and vertex-ID indexes are reused;
edge-ID indexes are non-unique and do not change graph IDs, properties, or
projection behavior.

For an existing database, prepare missing indexes **before upgrading**. Startup
checks the actual catalog definitions and usable state, including already-applied
migrations, and refuses missing/invalid prerequisites. It never performs a large
blocking build on populated tables or repairs an existing AGE label at startup.
See [production preparation](deploy/README.md#graph-index-preparation) for the
separate, opt-in operator script and recovery policy. Adding these indexes does
not make the full graph sync incremental or establish its total runtime.

### Configuration

| Setting (`.env`) | Purpose |
| --- | --- |
| `HORIZON_EMBED_URL`, `HORIZON_EMBED_MODEL`, `HORIZON_EMBED_DIM=1536`, `HORIZON_EMBED_API_KEY` (`_API_KEY_HEADER`, `_BEARER`) | Embeddings for semantic and hybrid search, configured the same way as PilotSwarm's embedder. Example: an Azure OpenAI `text-embedding-3-small` deployment's `/embeddings?api-version=2024-10-21` URL. Without them, search uses full text only. |
| `GITHUB_TOKEN` (or in the shell) | GitHub Copilot models for the agents. `config/model_providers.json` lists them, with `github-copilot:gpt-6-luna` as the default. |
| `PGHACKER_INDEXER_PORT`, `PGHACKER_MCP_PORT` | Service ports (3101, 3102). |
| `PILOTSWARM_WORKER_CONCURRENCY` | Session turns the worker runs at once (default 8). |
| `PGHACKER_RATE_PER_MINUTE`, `PGHACKER_RATE_BURST` | pghacker-mcp's per-client rate limit. Every local agent shares 127.0.0.1, so the launcher allows 3000 requests a minute in bursts of 500; the server's own default for public clients is 120 and 40. |
| `PGHACKER_AGENT_MCP_URL` | Another pghacker-mcp for the local agents, such as a deployment's public `/mcp`, to evaluate it. |

`config/test-slices.toml` chooses the months, seed threads and code paths that local runs retrieve. In Azure, `PGHACKER_SOURCES=open` lets `add_source` take any list, months, threads or commit instead, and `follow` keeps a list current (deploy/README.md).

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
| Repair | `find_duplicates` (skips pairs already judged different), `mark_distinct`, `merge_authors`, `merge_extracts`, `undo_merge` |

Results cite Message-IDs, verbatim quotes and file paths. Email addresses are used only to match authors: they live in a table the reader can't access, and bodies are stored with addresses redacted.

### Code-search budgets and large archives

`search` and `grep` restrict their scans to the selected snapshot and paths.
Small scopes (at most 200,000 lines) and short or unindexable patterns use
file-first scans through `(file_id, line_no)`. Larger scopes with a required
literal of at least six consecutive ASCII letters or digits in every
alternative can use the trigram index. This is a performance hint only:
the exact Rust matcher still verifies the candidates.

The trigram index is shared by **all loaded snapshots**. A common term such
as `for` can have millions of postings even when one version and a small
folder are selected. File-first scans avoid enumerating those unrelated
postings, but a full snapshot still requires reading its own lines.
Partitioning or a snapshot-scoped index would be needed to remove that
remaining large-archive tradeoff.

The time budget is shared by candidate retrieval, verification, counts,
context and name suggestions: 4 seconds by default, at most 10 seconds.
`search` exposes `budget_ms`; the MCP `grep` tool uses the 4-second default.
The 200,000-candidate cap remains. `limit` pages the results **after** totals
are computed; it does not make a broad count cheap. Check `complete` before
using `search` totals; incomplete totals are lower bounds. Inverted counts
that cannot be established fail, and incomplete `grep` searches fail
without emitting a fabricated `0` or running a pipeline over partial data.
Single regex evaluations and in-memory pipeline stages cannot be interrupted
mid-operation, but exhaustion is checked before results are returned.

The read-only `query` tool keeps its separate 5-second SQL timeout and does
not rewrite arbitrary SQL. For a scoped code count, keep the text predicate
outside a file-first scan so PostgreSQL cannot move it into a global trigram
bitmap scan. Replace `123` below with the wanted `code_snapshots.id`:

```sql
WITH files AS MATERIALIZED (
    SELECT id, path FROM code_files WHERE snapshot_id = 123
)
SELECT count(*)
FROM (
    SELECT line FROM code_lines
    WHERE file_id = ANY(ARRAY(
        SELECT id FROM files WHERE path LIKE 'src/backend/access/heap/%'
    ))
    OFFSET 0
) scoped
WHERE scoped.line LIKE '%for (%';
```

Two performance-tuning programs, in their crates' `perf_tuning/` folders, run
through Cargo as examples. `crates/pghacker-codesearch/perf_tuning/profile_search.rs`
profiles common loop searches on an isolated, synthetic multi-snapshot
database, with the local test database configured:

```bash
cargo run --locked -p pghacker-codesearch --example profile_search
```

`crates/pghacker-replay/perf_tuning/profile_full.rs` compares the full PG19
loop cases with GNU grep (requires Docker and downloads the pinned source into
the ignored `.runtime` cache):

```bash
cargo run --locked -p pghacker-replay --example profile_full
```

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
npm run crawl -- --watch                          # or keep extracting as threads arrive, as Azure does
```

The worker runs `PILOTSWARM_WORKER_CONCURRENCY` session turns at once (8 by default; set it in `.env`), so keep `--concurrency` at or below it.

## Checks

```bash
npm test                 # launcher, configuration, readiness and evaluation helpers
npm run rust:check       # rustfmt, clippy -D warnings, and the Rust tests (they create temporary databases)
npm run smoke            # with the app running: both MCP servers, the portal, the worker and the agents
npm run eval             # with the corpus built: the 20-question evaluation (eval/questions.json, results in eval/RESULTS.md)
```

The code-search exactness replay lives in `crates/pghacker-replay`, against its own `pghacker_replay` database:

```bash
cargo run --release -p pghacker-replay -- prepare                       # create and migrate pghacker_replay
cargo run --release -p pghacker-replay -- pg19                          # 2,000 generated searches on the PG19 subset
PGHACKER_STUDY_DIR=<study> cargo run --release -p pghacker-replay -- study
```

The PG19 replay compares `search` and `grep` with ripgrep and with GNU grep in Docker: 2,000 of 2,000 match each. The study replay reruns recorded agent searches, which are private: they're read from outside the repository and its output stays under `.runtime/replay`. It matched ripgrep on all 10,540 replayable searches through `search` and 7,254 through `grep`, which is byte-identical to GNU grep on 7,277 command lines.

## Deploy to Azure

[deploy/README.md](deploy/README.md) deploys the app to Azure Container Apps with Terraform. Start with the synthetic [example.tfvars](deploy/environments/example.tfvars); real environment and backend files are local and ignored. It runs as one image (`Dockerfile`), with HorizonDB and Azure AI Foundry behind private endpoints and Front Door in front. `/mcp` is public; the portal requires GitHub sign-in through your configured organization, team, or explicit login list. `npm run corpus:dump` exports the local corpus for the deployment's restore job.

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
| `scripts/` | The launcher, crawl and evaluation drivers; the container's supervisor, probe and corpus restore |
| `Dockerfile`, `deploy/` | The image, and the Azure deployment: Terraform, its environment and the runbook |
| `vendor/tokio-postgres` | A fork of tokio-postgres 0.7.18 (adds `bind_text`) |
