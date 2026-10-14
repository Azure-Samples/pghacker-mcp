# pghacker-mcp

pghacker-mcp gives AI agents cited answers about PostgreSQL's design history and source code. It is a [PilotSwarm](https://github.com/microsoft/PilotSwarm) **0.8.2** app with three parts (see [docs/design.md](docs/design.md)):

- **pghacker-mcp**: a public, stateless MCP server in async Rust. It serves the pgsql-hackers corpus and the PostgreSQL source through a read-only database role.
- **pghacker-indexer**: an internal async Rust service. Its workers retrieve sources (monthly mbox archives, seed threads, git), parse, thread, embed and project them into an Apache AGE graph. Its MCP tools serve the crawler sessions.
- **Crawler sessions**: PilotSwarm agents in `plugin/` that extract features, decisions and author positions, deduplicate them, and submit them with verbatim citations.

Locally, it runs on test slices of the data:
- two months of pgsql-hackers (June–July 2026);
- whole threads on SQL/PGQ and SQL/JSON;
- PostgreSQL's commit history, which you build from a clone ([Commit history](#commit-history)), of the whole tree or of a few folders.

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
projection behavior. The commit history's labels `BACKPATCH_OF` and `COMMITTED`
are such labels: bootstrap creates them empty, with AGE's `start_id` and `end_id`
indexes and the `id` index, so an existing database needs no preparation for them.

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
| `PGHACKER_CODE_HISTORY` | The schema whose commit history the code tools read, such as a build's before it is installed. Unset, `pghacker`, where `history build --install` puts it. |
| `PGHACKER_EXTRACTION_ORDER`, `PGHACKER_EXTRACTION_NEW_MAIL_SHARE`, `PGHACKER_EXTRACTION_NEW_MAIL_DAYS` | [The order of extraction](#the-order-of-extraction): `lanes` (the default) or `largest`; new mail's share of the threads leased (0.5) and its window in days (7). |

`config/test-slices.toml` chooses the months and seed threads that local runs retrieve. In Azure, `PGHACKER_SOURCES=open` lets `add_source` take any list, months, threads or commit instead, and `follow` keeps a list current (deploy/README.md).

## Tools

**pghacker-mcp**:

| Area | Tools |
| --- | --- |
| Mail | `search_threads`, `get_thread`, `search_extracts`, `author_perspective`, `search_authors`, `get_author`. The search tools take `mode`: `fulltext` (BM25), `semantic` (embeddings) or `hybrid` (default). |
| SQL and graph | `get_ontology` describes the data. `query` runs one read-only SQL statement, including Cypher on `pghacker_graph`, and returns CSV. |
| Code | `search` is a first-class code search. `grep` runs grep command lines. `read_file` and `list_files` read the source. They read any commit of the [commit history](#commit-history), as do `blame`, `get_commit` and `search_commits`. `search_patches` finds lines in the patches posted to the lists, by thread, and the commits that landed them. |

**pghacker-indexer**:

| Area | Tools |
| --- | --- |
| Sources and progress | `add_source`, `remove_source`, `list_sources`, `ingest_status` |
| Extraction | `next_extraction_batch`, `read_messages`, `match_extracts`, `submit_extracts` |
| Repair | `find_duplicates` (pairs the indexer finds as extracts arrive; skips pairs already judged different), `mark_distinct`, `merge_authors`, `merge_extracts`, `undo_merge` |

Results cite Message-IDs, verbatim quotes and file paths. Text keeps the email addresses written in it: subjects, bodies, patches and senders' names, a sender without a name showing as their address. The table that matches senders' addresses to authors, `author_emails`, isn't readable by pghacker-mcp, and the graph doesn't read it.

Mail stored before ingest kept addresses has `[email]` in their place, and senders' names without them. With the indexer running, `cargo run --release -p pghacker-indexer -- restore-addresses`, or `SELECT * FROM pghacker.restore_addresses()` as `pgh_indexer`, queues jobs that read every stored month again (from the cache when it kept the mbox), and fetch the messages from thread pages one by one. They restore only text stored exactly as ingest once made it from the archive's; chunks they change are embedded again, and patches indexed again. `ingest_status` shows the jobs.

### Code-search budgets and large archives

`search` and `grep` restrict their scans to the selected commit and paths.
Small scopes (at most 200,000 lines) and short or unindexable patterns use
file-first scans through `(file_id, line_no)`. Larger scopes with a required
literal of at least six consecutive ASCII letters or digits in every
alternative can use the trigram index. This is a performance hint only:
the exact Rust matcher still verifies the candidates.

The trigram index is shared by **every commit**: it indexes each line
instance of the history once. A common term such as `for` can have millions
of postings even when one version and a small folder are selected. File-first
scans avoid enumerating those unrelated postings, but a whole tree still
requires reading its own lines.

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
bitmap scan. A file version's lines are its lineage's instances alive at the
version's `lo` (get_ontology describes the tables); here, at REL_17_STABLE's head:

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

Two performance-tuning programs, in their crates' `perf_tuning/` folders, run
through Cargo as examples. `crates/pghacker-codesearch/perf_tuning/profile_search.rs`
profiles common loop searches on an isolated, synthetic commit history (a
master and eight release branches), with the local test database configured:

```bash
cargo run --locked -p pghacker-codesearch --example profile_search
```

`crates/pghacker-replay/perf_tuning/profile_full.rs` compares the full PG19
loop cases with GNU grep (requires Docker and downloads the pinned source into
the ignored `.runtime` cache):

```bash
cargo run --locked -p pghacker-replay --example profile_full
```

### How the graph follows the tables

The indexer projects the tables into the AGE graph `pghacker_graph`. Triggers
on the tables it projects record, in each writing transaction, the keys of the
vertices and edges the change may affect (`graph_changes`). The `graph_sync`
job applies them in batches of up to 1,000, each in its own short
transaction. It recomputes just those vertices and edges from the tables, and
the edges of any vertex that enters or leaves the graph, such as a reply whose
parent arrives later. Writers only append to the log, so they never wait on
the graph.

A full sync recomputes the whole graph instead: once after the change capture
is installed, after a `TRUNCATE` (as a corpus restore does), after an upgrade
that changes what the graph projects, weekly as a reconcile that warns if it
finds drift, and on request with `pghacker-indexer graph-reconcile`.
`ingest_status` shows the label counts the sync keeps, the changes waiting and
the last full sync.

The [commit history](#commit-history), once installed, joins the graph: a
`Commit` vertex for every commit, keyed by its full SHA, with its subject, date,
author name, branch and back-patch group; `MODIFIES` to the files it changed,
`DISCUSSED_IN` to the threads that discussed it, `BACKPATCH_OF` from each
commit of a back-patch group to the group's first, and `COMMITTED` from the one
mail author whose name, ignoring case, is the commit's author name (in
PostgreSQL's history, the committer's). Mail's citations of a commit join its
vertex once `resolve_anchors` resolves them to the full SHA. The history's file
versions give the `File` vertices (`loaded` when the history has the path, else
named by mail or an extract), their `Subsystem` vertices and the `PART_OF`
edges. The history's tables have no triggers, since an install swaps them in
whole: its writers record their own changes in the same transaction, namely a
sync's new commits and their files, the links that a refresh adds or drops,
and the commits whose back-patch group a regroup changes. An install asks for
a full sync. Each batch also checks the history's author names, so that
`COMMITTED` follows the authors' merges and renames. Without a history, nothing
more is projected.

### The order of extraction

`next_extraction_batch` leases threads from two lanes. A thread needs
extraction when extraction never read it, or read or skipped it before its
latest messages came. Those whose latest message is from the last 7 days are
**new mail**, leased oldest first, as mail arrives. All the others are the
**backfill**, leased from the newest back to the archive's start, so the
extracted months fill in from the present backward. While both lanes have
threads, new mail gets at most half of the threads leased, and a lane without
threads leaves the other every one. A thread whose mail changes after
extraction keeps its place in time.

`ingest_status` shows both lanes (`extraction.lanes`): the threads waiting and
leased, new mail's oldest activity, and the backfill's newest, after which
every message has been read except in new mail.
`PGHACKER_EXTRACTION_NEW_MAIL_DAYS` (7) and
`PGHACKER_EXTRACTION_NEW_MAIL_SHARE` (0.5) set the window and the share.
`PGHACKER_EXTRACTION_ORDER=largest` brings back the order before the lanes:
the threads never extracted first, the largest first.

### How duplicates are found

The `duplicates` job compares each new feature and decision with the live
ones of its kind, by its embedding's nearest neighbours (the HNSW index) and
by its name's trigrams, and keeps the pairs at least 0.8 alike by embedding
or 0.6 by name (`duplicate_candidates`). Each pair is compared once, when the
later of its two arrives, so the work grows with the new extracts rather than
with every pair. `find_duplicates` lists the kept pairs whose two extracts are
both live, except pairs judged different, and `pending` counts the extracts
not compared yet. An extract stored before its embedding is compared again
once it has one, and one whose merge is undone is compared again with the
extracts that came meanwhile.

### Tool calls' limits

The indexer's tool calls have database connections of their own, apart from
the job workers', so that no call can keep the jobs waiting: 8 by default
(`PGHACKER_TOOL_CONNECTIONS`). Each statement of a call may run 30 seconds
(`PGHACKER_TOOL_TIMEOUT_SECONDS`), and a call that waits 10 seconds for a
connection fails as busy. When a call's client goes away, the indexer cancels
the call's statements and closes their connections. `add_source` and
`remove_source` run to the end regardless: a change of sources, once asked
for, is made.

### Commit history

The commit history stores every line of every commit of every branch once
(the weave), and the code tools read any commit from it; [the search
design](docs/search-design.md) describes it. It is their only source: until
one is installed, they have no code to read. As the administrator:

```bash
# A bare clone with every branch and tag.
git clone --mirror https://git.postgresql.org/git/postgresql.git .runtime/postgres.git
# Build into a schema of its own (6 minutes), and swap it into pghacker.
cargo run --release -p pghacker-indexer -- history build --git-dir .runtime/postgres.git \
    --url https://git.postgresql.org/git/postgresql.git --install
# Or keep only some folders, for a smaller local build (repeat --path):
#   ... history build ... --install --path src/backend/parser --path src/include/nodes
# Rebuild every file of the branch heads and 40 random commits; compare with git.
cargo run --release -p pghacker-indexer -- history verify --git-dir .runtime/postgres.git \
    --schema pghacker --heads --sample 40
```

The running indexer and pghacker-mcp read it at once (in Azure, see
[deploy/README.md](deploy/README.md#commit-history)). To read a build before
installing it, set `PGHACKER_CODE_HISTORY` to its schema in `.env` and restart.
`history build --fetch` fetches or updates the bare clone from `--url` itself,
as the Azure job does.
The hourly `code_history_sync` job
fetches and applies new commits, and refreshes back-patch groups and the
links from commits to threads; `history sync` and `history regroup` do it by
hand. The `patch_lines` job indexes the mail's patches for `search_patches`
whether or not the history is installed. `add_source` for a PostgreSQL
version loads nothing: it names the commit the history already has, or starts
the sync when the history lacks it.

An installed history joins the graph
([How the graph follows the tables](#how-the-graph-follows-the-tables)), and
mail's commit citations join their commits. PostgreSQL's history adds about
750,000 vertices and edges. On a laptop, the full sync that
the install asks for took 36 seconds, and a full sync of the whole graph takes
about 20 seconds more than without it.

In the `pghacker-replay` crate's `perf_tuning/` folder, `history_parity`
checks a history against git on random commits: `get_commit` against
`git show`, its line counts, and `blame` against `git blame`.
`profile_history` times one code-tool call. Both read the history in
`--schema` (default `pghacker`) of `--database` (default the app's).

## Build the corpus

With the app running:

1. Add the sources. Ask a `pghacker-crawler` session in the portal, or call the indexer's `add_source` with `{kind: mailing_list, name: pgsql-hackers}`. The code comes from the [commit history](#commit-history), built once.
2. Watch the progress with `ingest_status`. Workers download the mbox months and seed threads (2 requests/s, cached in `.runtime/cache`), embed chunks, resolve file anchors against the commit history and sync the graph.
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
npm run eval             # with the corpus built: the 30-question evaluation (eval/questions.json, results in eval/RESULTS.md)
```

Rust integration tests read the local database configuration from `.env`, with
process variables taking precedence. The Rust-created operator fixture forwards
its scoped reader URL to Node; exporting `PGH_READER_PASSWORD` is not required.

The code-search exactness replay lives in `crates/pghacker-replay`, against its own `pghacker_replay` database:

```bash
cargo run --release -p pghacker-replay -- prepare                       # create and migrate pghacker_replay
cargo run --release -p pghacker-replay -- pg19                          # 2,000 generated searches on the PG19 subset
PGHACKER_STUDY_DIR=<study> cargo run --release -p pghacker-replay -- study
```

Each workload reads one commit from a history of its own (schema `replay_pg19`, or `replay_study_<repo>`): a one-commit repository with exactly that commit's blobs, built once and reused while the commit is the same. The PG19 replay compares `search` and `grep` with ripgrep and with GNU grep in Docker: 2,000 of 2,000 match each. The study replay reruns recorded agent searches, which are private: they're read from outside the repository and its output stays under `.runtime/replay`. It matched ripgrep on all 10,540 replayable searches through `search` and 7,254 through `grep`, which is byte-identical to GNU grep on 7,277 command lines.

## Deploy to Azure

[deploy/README.md](deploy/README.md) deploys the app to Azure Container Apps with Terraform. Start with the synthetic [example.tfvars](deploy/environments/example.tfvars); real environment and backend files are local and ignored. It runs as one image (`Dockerfile`), with HorizonDB and Azure AI Foundry behind private endpoints and Front Door in front. `/mcp` is public; the portal requires GitHub sign-in through your configured organization, team, or explicit login list. `npm run corpus:dump` exports the local corpus for the deployment's restore job.

## Repository

| Path | Contents |
| --- | --- |
| `crates/pghacker-core` | Config, pools, bootstrap and migrations, embeddings, search modes, MCP plumbing |
| `crates/pghacker-mail` | Archive client, mbox and MIME parsing, threading, authors, patches, anchors, chunks |
| `crates/pghacker-code` | The commit history's builder and sync |
| `crates/pghacker-codesearch` | pg_trgm code search, the grep translator, `read_file`, `list_files`; the history engine, `blame`, `get_commit`, `search_commits`, `search_patches` |
| `crates/pghacker-sqlquery` | The read-only `query` tool |
| `crates/pghacker-api` | Mail and graph tools |
| `crates/pghacker-indexer`, `crates/pghacker-mcp` | The two services |
| `crates/pghacker-replay` | The exactness replay harness and the history parity check |
| `migrations/`, `db/` | Schema and the PostgreSQL image |
| `plugin/` | Branding, agents and the MCP catalog |
| `scripts/` | The launcher, crawl and evaluation drivers; the container's supervisor, probe and corpus restore |
| `Dockerfile`, `deploy/` | The image, and the Azure deployment: Terraform, its environment and the runbook |
| `vendor/tokio-postgres` | A fork of tokio-postgres 0.7.18 (adds `bind_text`) |
