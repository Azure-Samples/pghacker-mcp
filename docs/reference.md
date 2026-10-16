# pghacker-mcp reference

Settings, tools and internals, for looking things up. The [README](../README.md) gets you started.

## Configuration

`npm run setup` creates `.env`, readable only by you, with random passwords; it keeps an existing `.env` and adds any missing password. Settings marked * may also come from the shell.

| `.env` | Purpose |
| --- | --- |
| `POSTGRES_USER`, `POSTGRES_DB`, `POSTGRES_PASSWORD`, `POSTGRES_PORT`, `DATABASE_URL` | The local database (port 55433) and its administrator's URL |
| `PGH_INDEXER_PASSWORD`, `PGH_READER_PASSWORD` | The passwords of `pgh_indexer`, which writes schema `pghacker` and the graph, and `pgh_reader`, which pghacker-mcp reads them as |
| `PORTAL_PORT`, `PGHACKER_INDEXER_PORT`, `PGHACKER_MCP_PORT` | Ports: 3001 for the site (the home page, the portal and `/mcp`), 3101 and 3102. The other `PORTAL_*` settings and `WORKERS` keep their local values |
| `PGHACKER_PORTAL_INTERNAL_PORT` | PilotSwarm's portal, behind the site: 3002 |
| `GITHUB_TOKEN` or `GH_TOKEN`* | GitHub Copilot models for the agents, listed in `config/model_providers.json`; `github-copilot:gpt-6-luna` is the default |
| `HORIZON_EMBED_URL`, `_MODEL`, `_DIM=1536`, `_API_KEY` (or `_API_KEY_HEADER`, `_BEARER`)* | Embeddings, set as for PilotSwarm, such as an Azure OpenAI `text-embedding-3-small` deployment (see [.env.example](../.env.example)). Without them, hybrid search uses full text only, and semantic search is unavailable |
| `PILOTSWARM_WORKER_CONCURRENCY`, `PILOTSWARM_ORCHESTRATION_CONCURRENCY` | Session turns and orchestrations run at once: 8 each, 1 to 64 |
| `PGHACKER_RATE_PER_MINUTE`, `PGHACKER_RATE_BURST` | pghacker-mcp's rate limit per client. Local agents all come from 127.0.0.1, so the launcher allows 3,000 requests a minute in bursts of 500; the server's default is 120 and 40 |
| `PGHACKER_AGENT_MCP_URL`* | Another pghacker-mcp for the local agents, such as a deployment's, to evaluate it |
| `PGHACKER_CODE_HISTORY`* | The schema of the [commit history](#commit-history) that the code tools read: `pghacker` by default |
| `PGHACKER_EXTRACTION_ORDER`, `PGHACKER_EXTRACTION_NEW_MAIL_SHARE`, `PGHACKER_EXTRACTION_NEW_MAIL_DAYS`* | [Extraction](#extraction)'s order, `lanes` (the default) or `largest`, and new mail's share (0.5) and window (7 days) |

`config/test-slices.toml` chooses the months and threads that local runs retrieve.

The launchers give each process only the settings it needs (`scripts/lib/config.mjs`, `scripts/lib/azure.mjs`). Run on their own, outside the launchers, the services also read these from their environment:

| Area | Settings, with their defaults |
| --- | --- |
| Database | `PGHACKER_ADMIN_DATABASE_URL`, else `DATABASE_URL`; `PGHACKER_INDEXER_DATABASE_URL` and `PGHACKER_READER_DATABASE_URL`, else the administrator's with the role's password; `PGH_SWARM_PASSWORD`, for PilotSwarm's own role in Azure; `PGHACKER_DB_CA_FILE`, more CA certificates for TLS |
| Listeners | `PGHACKER_INDEXER_ADDR` (`127.0.0.1:3101`); `PGHACKER_MCP_ADDR` (`127.0.0.1:3102`); `PGHACKER_MCP_INTERNAL_ADDR`, a listener without the rate limit, for agents; `PGHACKER_ALLOWED_HOSTS`, the Host headers accepted (loopback; `*` for any) |
| Clients | Behind a proxy, `PGHACKER_CLIENT_IP_HEADER` names the header with the client's address (in Azure, Front Door's `x-azure-socketip`), or `PGHACKER_TRUST_FORWARDED` takes the first X-Forwarded-For address; `PGHACKER_FRONT_DOOR_ID` is the one Front Door profile to serve |
| Connections | The indexer's `PGHACKER_WORKERS` (5 job workers), `PGHACKER_TOOL_CONNECTIONS` (8) and `PGHACKER_TOOL_TIMEOUT_SECONDS` (30); pghacker-mcp's `PGHACKER_MCP_POOL` (16) and `PGHACKER_QUERY_POOL` (8, for `query`) |
| Indexer | `PGHACKER_SOURCES`: `test-slices`, or `open` to take any source, as in Azure; `PGHACKER_SLICES` (`config/test-slices.toml`); `PGHACKER_EXTRACTION`: `manual`, or `automatic` when a scheduler extracts; `PGHACKER_GRAPH_RECONCILE_HOURS` (168; 0 for none) |
| Archive | `PGHACKER_ARCHIVE_BASE_URL` (`https://www.postgresql.org`), `PGHACKER_ARCHIVE_USER` and `PGHACKER_ARCHIVE_PASSWORD` (the published `archives` and `antispam`); `PGHACKER_HTTP_RATE` (2 requests a second), `PGHACKER_HTTP_USER_AGENT`; `PGHACKER_CACHE_DIR` (`.runtime/cache`), `PGHACKER_GIT_DIR` (`.runtime/git`), `PGHACKER_KEEP_DOWNLOADS` (true) |
| Logs | `RUST_LOG`: the launchers set `info,tokio_postgres=warn,rmcp=warn` |
| Usage telemetry | pghacker-mcp's `PGHACKER_USAGE_ENDPOINT`, the ingestion endpoint's origin (unset, no [telemetry](../PRIVACY.md); `npm start` never sets it); `PGHACKER_USAGE_IKEY`, the Application Insights resource's instrumentation key; `PGHACKER_USAGE_ID_KEY`, the key of the users' keyed hashes, at least 32 characters; `CONTAINER_APP_REVISION`, the instance |

`npm run crawl` reads `PGHACKER_CRAWL_MODEL` (`github-copilot:gpt-6-luna`), `PGHACKER_CRAWL_PER_SESSION` (8 threads) and, with `--watch`, `PGHACKER_CRAWL_START_SESSIONS` (else `--concurrency`) and `PGHACKER_CRAWL_MAX_SESSIONS` (16). Tests and local tools read the `.env` of the repository root, or of `PGHACKER_ROOT`.

## Running locally

`npm run setup` downloads and verifies PilotSwarm's release, installs the dependencies, the home page's too, builds the services and the home page, and creates `.env`. `npm start` then:

1. starts PostgreSQL 17, whose image (`db/Dockerfile`) adds AGE 1.7, pgvector 0.8 and pg_textsearch 1.5;
2. rebuilds the services if needed, and builds the home page;
3. bootstraps the roles, extensions, schema and graph, and migrates;
4. starts both services, restarting either if it stops, with logs in `.runtime/logs`;
5. starts PilotSwarm's worker and portal;
6. serves them on one origin, `PORTAL_PORT` (3001), routed as oauth2-proxy routes them in Azure (`scripts/lib/routes.mjs`): the home page at `/`, the portal at `/portal/` and pghacker-mcp at `/mcp`.

PilotSwarm listens behind the site, on `PGHACKER_PORTAL_INTERNAL_PORT` (3002). Version 0.8.2 has no base path ([microsoft/PilotSwarm#179](https://github.com/microsoft/PilotSwarm/issues/179)), so its page loads at `/portal/` but its files and API stay at `/assets/` and `/api/`, and its share links (`/?session=...`) are sent on to `/portal/`.

Each process gets only its own credentials: the indexer `pgh_indexer`, pghacker-mcp `pgh_reader` (read-only, with a 10-second statement timeout), and the portal only the two MCP URLs. With the app stopped, `npm run db:reset` recreates schema `pghacker` and the graph, and keeps PilotSwarm's data.

## The home page

The home page (`home/`) is a React, Vite and TypeScript page. It gives the MCP URL on its own origin, how to add it to an agent, and example prompts with the answers the tools gave: keep those in line with the tools when changing them. It is light unless a visitor picks dark, or the system's scheme, from its theme menu. Its files are under `/static/`, not `/assets/`, which is PilotSwarm's. In Azure, a small server (`scripts/home.mjs`) serves them behind oauth2-proxy, without sign-in. `npm --prefix home run dev` serves the page with hot reload on http://localhost:5173, and passes the rest of the site to a running `npm start`.

## Tools

The [README](../README.md#use-it) lists pghacker-mcp's tools, and each tool describes its arguments to MCP clients. The mail searches take `mode`: `fulltext` (BM25), `semantic` (embeddings) or `hybrid` (the default). The indexer's tools serve the crawler agents:

| Area | pghacker-indexer's tools |
| --- | --- |
| Sources | `add_source`, `remove_source`, `list_sources`, `ingest_status` |
| Extraction | `next_extraction_batch`, `read_messages`, `match_extracts`, `submit_extracts` |
| Repair | `find_duplicates`, `mark_distinct`, `merge_authors`, `merge_extracts`, `undo_merge` |

### Limits

`search`, `grep`, `search_patches` and `search_commits` take `budget_ms`: 10 seconds by default, at most 15. It replaces `pgh_reader`'s statement timeout for the call, and is checked between steps, so the step in progress can run past it. `search` and `grep` read at most 200,000 candidate lines. Past the budget or that cap, `search` gives its totals as lower bounds (`complete` is false), and `grep` fails rather than print partial output. `limit` pages the results after the totals are counted, so it doesn't make a broad count cheap. `query` has a 10-second timeout, 5 seconds to get a connection and 20 for the whole call.

| Tool | Limits |
| --- | --- |
| `search` | 100 results by default, at most 2,000. At one commit, context of at most 20 lines, and lines cut at 1,000 characters; with `history`, whole lines, without context |
| `grep` | Output cut at 1 MiB |
| `read_file`, `blame` | 2,000 lines; `read_file` also 200 KB, but always its first line whole |
| `list_files` | 500 entries by default, at most 5,000 |
| `get_commit` | A diff of 3,000 lines; context 3 lines by default, at most 1,000 |
| `search_commits` | 30 commits by default, at most 200; `changed_lines` and `occurrences` read at most 300,000 lines, and show 10 per commit |
| `search_patches` | 20 threads by default, at most 100, with 10 lines each; at most 50,000 matching lines and 20,000 patches read. Patches not yet indexed aren't searched: a search that finds nothing counts them |
| `query` | 100 rows by default, at most 1,000, and 1 MiB of CSV; 64 KiB of SQL |

`search_commits` and `search_patches` say `incomplete` when the budget or a cap cut them short. The [search design](search-design.md#indexes-and-queries) explains the plans behind these limits.

For a scoped count with `query`, read the scope's lines first and filter them after, so that PostgreSQL doesn't scan the trigram index, which covers every commit. Here, `for (` in `src/backend/access/heap` at REL_17_STABLE's head:

```sql
WITH files AS MATERIALIZED (
    SELECT v.lineage, v.lo
    FROM code_refs r JOIN code_file_versions v
      ON v.lo <= r.addr AND r.addr < v.hi AND (v.alive IS NULL OR v.alive @> r.addr)
    WHERE r.kind = 'branch' AND r.name = 'REL_17_STABLE'
      AND v.path LIKE 'src/backend/access/heap/%'
)
SELECT count(*)
FROM (
    SELECT i.line FROM files f JOIN code_line_instances i
      ON i.lineage = f.lineage AND i.lo <= f.lo AND f.lo < i.hi
         AND (i.alive IS NULL OR i.alive @> f.lo)
    OFFSET 0
) scoped
WHERE scoped.line LIKE '%for (%';
```

## Email addresses

Text keeps the email addresses written in it: subjects, bodies, patches and senders' names. `author_emails`, which matches senders' addresses to authors, isn't readable by pghacker-mcp, and the graph doesn't use it.

Mail stored before ingest kept addresses has `[email]` in their place. With the indexer running, `cargo run --release -p pghacker-indexer -- restore-addresses` (with `.env`'s settings in the environment), or `SELECT * FROM pghacker.restore_addresses()` as `pgh_indexer`, queues jobs that read that mail from the archive again. They restore only text still as ingest stored it, and embed changed chunks again; `ingest_status` shows them.

## Commit history

The commit history stores every line of every commit of every branch once, and is the code tools' only source; the [search design](search-design.md) describes it. The [README](../README.md#load-the-data) builds it. The `history` commands run as the administrator (`DATABASE_URL` in the environment):

- `history build ... --path src/backend/parser` keeps only some folders (repeat `--path`), and `--fetch` fetches the clone from `--url` itself;
- without `--install`, `history build` leaves the build in schema `pghacker_history`: set `PGHACKER_CODE_HISTORY=pghacker_history` and restart to read it;
- `history verify --git-dir .runtime/postgres.git --schema pghacker --heads --sample 40` rebuilds every file of the branch heads and of 40 random commits, and compares them with git;
- `history sync` and `history regroup` do by hand what the hourly `code_history_sync` job does: apply new commits, and refresh the back-patch groups and the links from commits to threads.

The `patch_lines` job indexes the mail's patches for `search_patches`, even before a history is installed. `add_source` for a PostgreSQL version loads nothing: it names the history's commit, or starts the sync when the history lacks it. In `crates/pghacker-replay/perf_tuning/`, `history_parity` compares `get_commit` and `blame` with git on random commits, and `profile_history` times one code-tool call.

## The graph

The indexer projects the tables into the AGE graph `pghacker_graph`. Triggers record the vertices and edges each change may affect (`graph_changes`), and the `graph_sync` job recomputes them in short batches of up to 1,000; writers only append to that log, so they never wait on the graph. A full sync recomputes the whole graph: after a restore, an upgrade that changes the graph, or a history install; weekly, as a reconcile that warns of drift; and on request, with `pghacker-indexer graph-reconcile`. `ingest_status` shows the label counts, the changes waiting and the last full sync.

The commit history adds a `Commit` vertex per commit, keyed by its full SHA, with `MODIFIES` to its files, `DISCUSSED_IN` to its threads, `BACKPATCH_OF` from each other commit of a back-patch group to its first, and `COMMITTED` from the mail author whose name, ignoring case, is the commit's author name. Mail's citations of a commit join its vertex once `resolve_anchors` resolves them to the full SHA. The history's file versions give the `File` and `Subsystem` vertices and the `PART_OF` edges. Its tables have no triggers: its writers record their own changes.

Bootstrap creates each edge label with its indexes. Startup refuses a database whose graph indexes are missing or invalid, rather than build them on full tables: prepare them before upgrading ([graph index preparation](../deploy/README.md#graph-index-preparation)).

## Extraction

`next_extraction_batch` leases the threads that extraction never read, or whose mail changed since, from two lanes: **new mail**, whose latest message is from the last 7 days, oldest first; and the **backfill**, from the newest back to the archive's start. While both have threads, new mail gets at most half the leases. A thread whose mail changes during its lease is leased again. With `PGHACKER_EXTRACTION_ORDER=largest`, the largest threads never extracted go first instead. `ingest_status` shows both lanes (`extraction.lanes`).

Extracting a thread again replaces its participants' positions there, except those a merge moved, which `undo_merge` needs. A feature's release comes from its thread with the latest message.

`npm run crawl` drives extraction locally: `--supervisor` lets the pghacker-crawler agent run the pilot, and `--watch` keeps extracting as threads arrive, as in Azure. Keep `--concurrency` at or below `PILOTSWARM_WORKER_CONCURRENCY`.

**Duplicates.** The `duplicates` job compares each new feature and decision with the others of its kind, once per pair, and keeps the pairs at least 0.8 alike by embedding or 0.6 by name. `find_duplicates` lists them, except pairs judged different (by `mark_distinct`, or an extractor's `distinct_because`); `pending` counts the extracts not compared yet.

**Tool calls.** The indexer's tool calls have connections of their own (8), so they can't hold up the jobs. Each statement may run 30 seconds, and a call that waits 10 seconds for a connection fails as busy; at most two `find_duplicates` calls run at once. A call whose client goes away is cancelled, except `add_source` and `remove_source`, which finish.

## Tests and profiling

`npm test` also runs the site's routes, the edge's and the home page's tests; `npm --prefix home test` runs the home page's alone. `npm run test:routes` sends requests through the real oauth2-proxy, with the container's flags, and checks which paths it lets through without sign-in, whatever forwarded headers a client forges, and where sign-in sends GitHub and comes back to; it downloads oauth2-proxy's release once, checked against its checksum. With the app running, `npm run smoke` checks the site's routes too.

The Rust tests create temporary databases on the local PostgreSQL, with the settings in `.env`, or the environment's first.

`crates/pghacker-replay` checks code search against ripgrep and GNU grep, in a database of its own:

```bash
cargo run --release -p pghacker-replay -- prepare   # create and migrate pghacker_replay
cargo run --release -p pghacker-replay -- pg19      # 2,000 generated searches on the PG19 subset
PGHACKER_STUDY_DIR=<study> cargo run --release -p pghacker-replay -- study   # recorded agent searches
```

The PG19 replay matches both on all 2,000 searches. The study replay reruns recorded agent searches, which are private: it reads them from outside the repository, the clones of the repositories they searched from the folders that `PGHACKER_REPLAY_<NAME>` variables name (`PGHACKER_REPLAY_SCENARIOS` for a repository named `scenarios`), and keeps its output under `.runtime/replay`. `PGHACKER_RG` names the replays' ripgrep.

Two profilers run as Cargo examples: `profile_search`, on a synthetic history, and `profile_full`, against GNU grep in Docker:

```bash
cargo run --locked -p pghacker-codesearch --example profile_search
cargo run --locked -p pghacker-replay --example profile_full
```

## Repository

| Path | Contents |
| --- | --- |
| `crates/pghacker-core` | Config, pools, bootstrap and migrations, embeddings, search modes, MCP plumbing, usage telemetry |
| `crates/pghacker-mail` | Archive client, mbox and MIME parsing, threading, authors, patches, anchors, chunks |
| `crates/pghacker-code` | The commit history's builder and sync |
| `crates/pghacker-codesearch` | The code tools: search, grep, files, blame, commits and patches |
| `crates/pghacker-sqlquery` | The read-only `query` tool |
| `crates/pghacker-api` | Mail and graph tools |
| `crates/pghacker-indexer`, `crates/pghacker-mcp` | The two services |
| `crates/pghacker-replay` | The search replays and the history's parity check |
| `migrations/`, `db/` | The schema, and the PostgreSQL image |
| `plugin/` | Branding, agents and the MCP catalog |
| `home/` | The public home page: React, Vite and TypeScript |
| `scripts/` | The launcher, the site's routes and its edge, crawl and evaluation drivers; the container's supervisor, the home page's server, probe and corpus restore |
| `Dockerfile`, `deploy/` | The image, and the Azure deployment |
| `vendor/tokio-postgres` | A fork of tokio-postgres 0.7.18, which adds `bind_text` |
