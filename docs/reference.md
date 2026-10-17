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
| Usage telemetry | pghacker-mcp's `PGHACKER_USAGE_ENDPOINT`, the ingestion endpoint's origin (unset, no [telemetry](../PRIVACY.md); `npm start` never sets it); `PGHACKER_USAGE_IKEY`, the Application Insights resource's instrumentation key; `PGHACKER_USAGE_ID_KEY`, the key of the users' keyed hashes, at least 32 characters; `CONTAINER_APP_REVISION`, the instance; `PGHACKER_USAGE_DETAILS` (false), whether events record what users type into tools: arguments' values, error messages and sqlparser's errors. Without it, a tool call keeps its arguments' names (`argumentNames`) and size and the values of its fixed-choice arguments (the schema's enums and booleans), and its events say `common.details` `off` |
| Feedback | `PGHACKER_FEEDBACK_RATE`, the chance from 0 (off, the default) to 1 that an address's hour of use brings a request for [feedback](#feedback), with usage telemetry only; `PGHACKER_FEEDBACK_COMMENTS` (false), whether its form takes a comment; `PGHACKER_FEEDBACK_QUESTION_GAP` (180 seconds) and `PGHACKER_FEEDBACK_SESSION_GAP` (600), the pauses over which a call starts a question and under which a session stays active; `PGHACKER_FEEDBACK_MIN_QUESTIONS` (3), the questions before the first ask; `PGHACKER_FEEDBACK_CLIENTS`, the apps asked: names, separated by commas, one of which an app's `clientInfo` name contains, ignoring case; `PGHACKER_FEEDBACK_DEBUG` (false), to try the form by hand |

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

The home page (`home/`) is a React, Vite and TypeScript page. It gives the MCP URL on its own origin, how to add it to an agent, and example prompts with the answers the tools gave: keep those in line with the tools when changing them. It is light unless a visitor picks dark, or the system's scheme, from its theme menu. Its footer links to the deployment's privacy statement only if the deployment has one: the page's server adds its URL (`PGHACKER_PRIVACY_URL`, an https URL; Terraform's `privacy_url`) to the page as a `privacy-url` meta tag, and the footer reads that tag. Without it, the page has no privacy link. Its top bar links to the GitHub repository that `index.html`'s `github-repo` meta tag names. The page's server adds the repository's stars to that tag once GitHub's API shows them, without a token, so only while the repository is public; it checks at most every 10 minutes, and no page waits for it (`scripts/lib/github-stars.mjs`). Its files are under `/static/`, not `/assets/`, which is PilotSwarm's. In Azure, a small server (`scripts/home.mjs`) serves them behind oauth2-proxy, without sign-in. `npm --prefix home run dev` serves the page with hot reload on http://localhost:5173, and passes the rest of the site to a running `npm start`.

## Tools

The [README](../README.md#use-it) lists pghacker-mcp's tools, and each tool describes its arguments to MCP clients. The mail searches take `mode`: `fulltext` (BM25), `semantic` (embeddings) or `hybrid` (the default). `get_ontology` lists the tables and columns that `query` reads, and the schema's comments describe each one (migration `0023`): `obj_description` and `col_description` read them, as `get_ontology` shows, and history builds and installs copy those of the history's tables. The indexer's tools serve the crawler agents:

| Area | pghacker-indexer's tools |
| --- | --- |
| Sources | `add_source`, `remove_source`, `list_sources`, `ingest_status` |
| Extraction | `next_extraction_batch`, `read_messages`, `match_extracts`, `submit_extracts` |
| Repair | `find_duplicates`, `mark_distinct`, `merge_authors`, `merge_extracts`, `undo_merge` |

### Query's read-only policy

`query` takes one SELECT, WITH, VALUES, TABLE, PostgreSQL SHOW or EXPLAIN of a read. It rejects data changes, DDL, maintenance and transaction control, including writes inside CTEs or EXPLAIN, SELECT INTO and all FOR UPDATE/SHARE row-locking variants. Caller-supplied SET/RESET, ALTER SYSTEM, role/session-authorization changes and `set_config()` fail with an error explaining how to inspect a setting instead. SHOW ALL, SHOW setting, `current_setting()` and catalog SELECTs remain available.

Known side-effecting functions are refused wherever they occur, including qualified/quoted names, casts, subqueries and table functions: session configuration (`set_config`, `setseed`, pg_trgm's `set_limit`), all advisory-lock variants, snapshot export, sleeps, notifications, sequence changes, backend/server/upgrade control, WAL/backup/replication changes, statistics resets, server files, large objects, dblink/FDW connection functions and AGE graph/label administration, graph generators, internal write-clause functions or file loading. BRIN/GIN maintenance, BM25 cache/compaction/test helpers and corpus repair/sync entry points (`graph_request_full`, `restore_addresses`, `rethread_mail`) are refused too; normal index-backed reads and BM25 query constructors remain available. SQL-interpreter functions such as `query_to_xml*`, `ts_stat` and `ts_rewrite` are also refused: express the underlying read directly as SQL. The name-level `ts_rewrite` rule includes its non-SQL-executing overload.

AGE's `cypher()` stays available with literal SQL-string or dollar-quoted query text and its typed result columns. Its text is checked for writing clauses (CREATE, MERGE, SET, DELETE, DETACH DELETE and REMOVE), procedure/loading forms and known dangerous function calls, without treating strings, comments, labels, properties or map keys as writes. Computed Cypher text is refused. EXPLAIN ANALYZE executes its query, so the inner SQL and Cypher receive the same checks; reporting options such as BUFFERS, WAL and SETTINGS do not change session settings.

Normal aggregates, JSON/text/date functions, `random()`, catalog inspection and vector/trigram/BM25 reads are not rejected merely for being unfamiliar, VOLATILE or named `pg_*`. For PostgreSQL/AGE syntax sqlparser cannot parse, query retains its first-word fallback **after token-aware checks of known unsafe forms**. Deep ASTs use those token checks too, rather than exhaust the server thread's stack during recursive inspection. This selective denylist and fallback are not a complete SQL/Cypher sandbox: trusted stored functions, views, operators and type input/output routines can execute code that a caller's syntax does not reveal.

Each statement that passes by its tokens is recorded, in the call's usage, as `McpQueryFallback`, with the `OperationId` of its call's `McpToolCall`: `fallback` says why (`unparsed`, sqlparser couldn't parse it, such as a `cypher()` call with its column definitions or PostgreSQL's `TABLE` statement; `too_deep`, it nests deeper than the 64 levels the AST checks go), and, with usage details (`PGHACKER_USAGE_DETAILS`), `parseError` holds sqlparser's error, at most 1,024 characters, which quotes the SQL. The log line, at info, says only why.

A call that fails in PostgreSQL has the error's SQLSTATE in its `McpToolCall` as `errorCode`, such as `42703` for an undefined column, or `57014` for the statement timeout, with or without details. Calls refused by these checks, and failures that aren't PostgreSQL's, such as a busy pool, have none.

The database remains the final boundary: the reader's privileges, a read-only transaction that is always rolled back, statement/call timeouts and result caps. Each query connection is cleaned before reuse, including session advisory-lock release; an unfinished call cancels and closes its connection. Trusted executor SET LOCAL setup and cleanup are not caller SQL. This parser policy does not change database EXECUTE privileges.

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

## Skills

pghacker-mcp serves [Agent Skills](https://agentskills.io/specification) as MCP's [skills extension](https://modelcontextprotocol.io/extensions/skills/overview) (SEP-2640, `io.modelcontextprotocol/skills`) defines. Its capabilities declare the extension and resources. `skills/list` and `skills/get` give each skill's frontmatter and the SHA-256 digest and size of each of its files, and resources/list and resources/read serve the files at `skill://<name>/<file>`. The skills are the repository's `skills/` folder, embedded in the binary when it's built; startup refuses a skill that breaks the Agent Skills rules or the extension's limits. Usage telemetry records each `skills/list`, `skills/get` and resource read, with its URI.

| Skill | What it does |
| --- | --- |
| [`review-as-author`](../skills/review-as-author/SKILL.md) | Reviews a patch, commit or design as a given pgsql-hackers author would: the points they would likely raise, each with a quote of their own message or one of their commits, in the third person and marked as a prediction. Points without their evidence stay apart, as general reviewer notes. With an as-of date, it uses only what they had said by then |

`author_perspective` serves the skill with two arguments: `paths`, a patch's files or folders, adds the author's positions on the features implemented there (`feature_files`); `until` keeps only positions whose cited messages, and messages, came before a date.

Few clients load skills from MCP servers yet; the extension's [client matrix](https://modelcontextprotocol.io/extensions/client-matrix) tracks them. Elsewhere:

- a client that reads MCP resources can attach `skill://review-as-author/SKILL.md` to a conversation;
- any agent that reads skills from folders can use a copy of `skills/review-as-author`, such as in `.claude/skills/` for Claude Code or `.github/skills/` for GitHub Copilot;
- the server's instructions name the skill and summarize its procedure, for agents that see only the tools.

`npm run eval` checks the skill against what authors actually said: `eval/reviews.json` has 10 patches posted to pgsql-hackers, each with the points its reviewer actually raised. An agent reviews each patch as that reviewer would have when it was posted, with the skill (`--skill skills/review-as-author/SKILL.md`, or its `skill://` URI, read from the server and checked against its digests) or without it, as a baseline. A review passes when a judge finds the reviewer's main point in it, its citations are verbatim, at least two quote the reviewer's own words, and none came on or after the patch's date. The [README](../README.md#check-it) has the command.

## Feedback

Now and then, pghacker-mcp asks an interactive client's user whether the last answer helped, through the client's own form ([PRIVACY.md](../PRIVACY.md)). The model takes no part: the tools, the instructions and the results don't change.

- **Questions:** the server follows each client, an address (the usage identity) and an app (`clientInfo.name`), by the pauses between its tool calls. A call that comes more than `PGHACKER_FEEDBACK_QUESTION_GAP` (180 s) after the client's last call ended starts a new question; while a call runs, the next belongs to the same question. A pause under `PGHACKER_FEEDBACK_SESSION_GAP` (600 s) keeps the session active.
- **The ask:** the server may hold a call that starts a question in an active session, when the client has asked `PGHACKER_FEEDBACK_MIN_QUESTIONS` (3) questions before it and the last one had a successful call. Instead of running it, it answers `input_required` (MCP 2026-07-28's multi round-trip requests) with an `elicitation/create` form: an empty message, as the client names the server, and a required rating, "Was the last answer helpful?", with no default: 👍 Helpful, 👎 Not helpful or Skip.
  - For each address and UTC clock hour, a keyed draw arms the hour with the chance `PGHACKER_FEEDBACK_RATE` (Terraform's `feedback_rate_per_hour`, 0.1). An armed hour has at most one ask, at its first such question.
  - The form's `requestState` is a random ask id sealed with the app and the tool, valid for 6 hours. It answers once, from whatever address the retry comes: behind a gateway such as Global Secure Access, a client reaches the server from a new address on each connection.
  - The client asks the user and retries the call with the answer and the state. The server records the answer, then runs the call whatever it is: a rating, a Skip, a decline, a cancel, or a state that isn't valid.
- **Who is asked:** only an app whose `clientInfo.name` contains one of the names in `PGHACKER_FEEDBACK_CLIENTS`, ignoring case, and whose requests declare form elicitation under 2026-07-28. Apps are told apart by their whole names.
  - The default names are `copilot` (Copilot CLI's and the GitHub Copilot app's `copilot-cli`), `codex` (`codex-mcp-client`), `Visual Studio Code` (and `Visual Studio Code - Insiders`), `claude` (Claude Code's `claude-code` and the Claude app's `claude-ai`) and `cursor` (`cursor-vscode`). A blank setting means these.
  - Never a request without a client identity (the internal listener), on an earlier protocol, or to a URL with `?feedback=off`, and such requests aren't followed either. `PGHACKER_AGENT_MCP_URL` gets `feedback=off`.
  - A decline pauses asks at the address for a day. An answer from another address than the ask's counts as the hour's ask, and a decline pauses, at both.
- **Events:**
  - The held call is an `McpToolCall` with `activityStatus` `InputRequired`. With it, in the same operation, comes `McpFeedbackAsked`:
    - properties `askId`, `toolName`, and the rated question's `questionStart` and `questionEnd`;
    - measurements `question` (its number for the client), `questionCalls`, `questionFailed` and `gapSeconds`.
  - The retry records `McpFeedback`, then its `McpToolCall`. `McpFeedback` has `askId`, `outcome` (`rated`, `skipped`, `declined` or `dismissed`), `rating` (`up` or `down`) and `answeredFrom`: the address it came from against the one asked, `same`, the same /24 (`same24`; for IPv6, the same /48), the same /16 (`same16`; /32), `other`, or `unknown` when the server no longer has the ask, as after a restart. With comments on, it also has the user's `comment` and its `commentLength`, whether or not usage records details. Its `UserId` is the address that answered. For that comparison only, the asked address's /24 (IPv6: /48) is kept in memory until the answer or the state's expiry.
  - `McpFeedbackRejected` records a retry whose state isn't the app's or the tool's, has expired (`reason` `invalid`), or was used (`used`), or one over a limit (`limit`). A retry from a client that can't be asked records nothing.
- **Limits:** one ask an hour per address; 3 outcomes per address an hour, 60 a minute per replica; rejections recorded once a minute per address, 60 a minute in all; with comments on, 500 characters. What follows the clients and keeps the asks rare lives in memory and is bounded, so a restart forgets it and the warm-up starts again; the draw doesn't change. When the memory is full of addresses still paused or asked, a new address isn't asked, rather than any pause being forgotten.
- **Rotating addresses:** behind a gateway that gives each connection its own address from a pool, such as Global Secure Access, one person is several addresses. Each counts its questions and warm-up apart and has its own draw, so such a person may be asked more than once an hour, though not at the address asked or the one that answered.
- **Off:** `PGHACKER_FEEDBACK_RATE=0`, or no usage telemetry: no call is held.
- **Debug mode:** `PGHACKER_FEEDBACK_DEBUG=true`, to try the form by hand, asks at every question after a client's first, however long the pause before it, with no warm-up, draw, one ask an hour, pause after a decline or limit on an address's outcomes. It logs, at info, why each call that isn't held isn't. Only the client list, form elicitation, `?feedback=off` and the question gap still apply. Terraform doesn't set it and the Azure supervisor doesn't pass it, so a deployment made with this repository can't turn it on.

The rated question's calls are the `McpToolCall` events with the ask's `UserId` and `clientName`, from `questionStart` to `questionEnd`; behind a rotating gateway, the calls made from its other addresses are missing. These queries read the results in `AppEvents`. Give the command the window to read, such as `az monitor app-insights query --apps <usage_app_id> --offset 30d --analytics-query '<query>'`: without `--offset` it reads only the last hour. For the thumbs-down query, the window must reach back to the rated questions, a few minutes before their asks.

```kql
// Asks, and what came of them, by client
let asks = AppEvents
    | where tostring(Properties.action) == "McpFeedbackAsked"
    | project askId = tostring(Properties.askId), client = tostring(Properties.clientName);
let answers = AppEvents
    | where tostring(Properties.action) == "McpFeedback"
    | project askId = tostring(Properties.askId), outcome = tostring(Properties.outcome),
              rating = tostring(Properties.rating);
asks
| join kind=leftouter answers on askId
| summarize asks = count(), up = countif(rating == "up"), down = countif(rating == "down"),
            skipped = countif(outcome == "skipped"), declined = countif(outcome == "declined"),
            dismissed = countif(outcome == "dismissed"), unanswered = countif(isempty(outcome)) by client
```

```kql
// The helpful rate by day
AppEvents
| where tostring(Properties.action) == "McpFeedback" and tostring(Properties.outcome) == "rated"
| summarize up = countif(tostring(Properties.rating) == "up"),
            down = countif(tostring(Properties.rating) == "down") by day = bin(TimeGenerated, 1d)
| extend helpful = round(100.0 * up / (up + down), 1)
```

```kql
// Thumbs-down, with the calls of the question each rated
let downs = AppEvents
    | where tostring(Properties.action) == "McpFeedback" and tostring(Properties.rating) == "down"
    | project askId = tostring(Properties.askId);
let asks = AppEvents
    | where tostring(Properties.action) == "McpFeedbackAsked"
    | project askId = tostring(Properties.askId), UserId, client = tostring(Properties.clientName),
              startedAt = todatetime(Properties.questionStart), endedAt = todatetime(Properties.questionEnd);
let calls = AppEvents
    | where tostring(Properties.action) == "McpToolCall" and tostring(Properties.activityStatus) != "InputRequired"
    | project TimeGenerated, UserId, client = tostring(Properties.clientName), tool = tostring(Properties.toolName),
              query = tostring(Properties["arg.query"]), version = tostring(Properties["arg.version"]);
downs
| join kind=inner asks on askId
| join kind=inner calls on UserId, client
| where TimeGenerated between (startedAt .. endedAt)
| project askId, TimeGenerated, tool, query, version
| order by askId asc, TimeGenerated asc
```

Without usage details, `query` and `version` are empty: the calls keep their tools and their arguments' names.

```kql
// Where answers came from, against the address asked
AppEvents
| where tostring(Properties.action) == "McpFeedback"
| summarize answers = count() by client = tostring(Properties.clientName), answeredFrom = tostring(Properties.answeredFrom)
```

A thumbs-down's calls can seed the evaluation: the maintainer words the question and finds its answer with the tools, then adds it to `eval/questions.json` by hand. No user text goes into the repository.

## Email addresses

Text keeps the email addresses written in it: subjects, bodies, patches and senders' names. `author_emails`, which matches senders' addresses to authors, isn't readable by pghacker-mcp, and the graph doesn't use it.

Mail stored before ingest kept addresses has `[email]` in their place. With the indexer running, `cargo run --release -p pghacker-indexer -- restore-addresses` (with `.env`'s settings in the environment), or `SELECT * FROM pghacker.restore_addresses()` as `pgh_indexer`, queues jobs that read that mail from the archive again. They restore only text still as ingest stored it, and embed changed chunks again; `ingest_status` shows them.

## Threading

A thread is the mail that In-Reply-To and References link. Ingest leaves out what old mailers wrote there that names no message: placeholders without an `@`, such as elm's `<no.id>`, and senders' addresses. exmh and MH-E wrote the sender replied to before the Message-ID (`Message from Tom Lane <tgl@sss.pgh.pa.us> of "…" <id>`), and other mailers copied the address into References, so replies to anything Tom wrote once shared a thread. `parent_message_id` and `references_ids` keep only the IDs that name messages.

A reply that starts a topic gets a thread of its own: it was sent 30 days or more after its parent, under a subject that isn't a reply, has another key than its parent's, and gives no version (a series re-posted under a new name with its `v12` continues its thread). Its replies follow it whichever arrives first, unless extracts cite them.

The `rethread_mail` job threads mail stored before these rules again. As `pgh_indexer`, `SELECT pghacker.rethread_mail();` (or `cargo run --release -p pghacker-indexer -- rethread-mail`) queues a dry run, which changes nothing: it reads the months whose stored IDs name what isn't a message from the archive again, plans the affected threads afresh, and records its report as the job's result (`SELECT jsonb_pretty(result) FROM pghacker.jobs WHERE kind = 'rethread_mail' AND key = 'dry-run'`): the groups of changes, the threads each makes or keeps, the corrected IDs, where extracts go, and the plan's ID. `SELECT pghacker.rethread_mail('<plan>');` (or `rethread-mail --apply <plan>`) then applies the groups that are still as the dry run found them, each in a transaction, and skips a group whose threads changed or that an extraction lease holds: a new dry run plans those again. Its result lists the threads to extract again. A thread keeps the messages linked to its root, and threads that ingest kept apart (it doesn't merge threads that extracts cite) stay apart unless corrected IDs link them. Extracts follow the messages they cite: a decision or position goes to the thread with most of its citations, a feature is linked to each thread with its citations, decisions or positions, and a feature's file goes with the patches that show it. New threads are pending, and threads that lost or gained messages get a new content version.

## Patch series

A patch's series version is the label it was posted under, as its file name or its message's subject writes it, without the `v`: `3`, `3.1`, `6a`, `01`, `20261003`, `12-pg13`. Its part is its number in the set, from the file name only: 1 for `v3-0001-…`. `search_patches` and `get_commit` show the version; `query` reads both in `patches`.

The rules (`crates/pghacker-mail/src/series.rs`) keep only the forms names use reliably and leave the rest out:

- a label before a part number (`v3-0001-…`, `v20220130-2-0001-…`, `vpg18-0001-…`); of a label over 24 characters, a slug, the first token (2 for `v2-Opclass-bitwise-equality-0001`);
- a version first, last or inside a name someone wrote (`fix-v3`, `fix_v3.1`, `brin-inclusion-v05-…`), with decimals of up to three digits (3 for `insert_on_dup.v3.2013_09_10`);
- a date (`fix-20251114`);
- a `0.x` decimal;
- a bare number where the thread's posts of that name rise with it (`psql-dp-3` … `psql-dp-5`);
- else the subject's `[PATCH v3]` tag, unless the message is a reply.

A release current when the patch was posted (`v9.6` in 2016) isn't a version, nor is `vN` glued to a word (`uuidv7`). `cargo run -p pghacker-mail --example patch_series -- <export.jsonl>` reports how every patch of an export parses, by rule. When the rules change, the `patch_series` job parses every stored patch again, once ([upgrading](../deploy/README.md#patch-series)).

## Commit history

The commit history stores every line of every commit of every branch once, and is the code tools' only source; the [search design](search-design.md) describes it. The [README](../README.md#load-the-data) builds it. The `history` commands run as the administrator (`DATABASE_URL` in the environment):

- `history build ... --path src/backend/parser` keeps only some folders (repeat `--path`), and `--fetch` fetches the clone from `--url` itself;
- without `--install`, `history build` leaves the build in schema `pghacker_history`: set `PGHACKER_CODE_HISTORY=pghacker_history` and restart to read it;
- `history verify --git-dir .runtime/postgres.git --schema pghacker --heads --sample 40` rebuilds every file of the branch heads and of 40 random commits, and compares them with git;
- `history sync` and `history regroup` do by hand what the hourly `code_history_sync` job does: apply new commits, and refresh the back-patch groups and the links from commits to threads.

The `patch_lines` job indexes the mail's patches for `search_patches`, even before a history is installed. The history isn't a source: `sources` lists the mailing lists, and there is no version to add, since the history holds every branch, tag and commit. `list_sources.history`, `ingest_status.code.history` and `get_ontology.loaded.code` report its repository, head and last sync. In `crates/pghacker-replay/perf_tuning/`, `history_parity` compares `get_commit` and `blame` with git on random commits, and `profile_history` times one code-tool call.

## The graph

The indexer projects the tables into the AGE graph `pghacker_graph`. Triggers record the vertices and edges each change may affect (`graph_changes`), and the `graph_sync` job recomputes them in short batches of up to 1,000; writers only append to that log, so they never wait on the graph. A full sync recomputes the whole graph: after a restore, an upgrade that changes the graph, or a history install; weekly, as a reconcile that warns of drift; and on request, with `pghacker-indexer graph-reconcile`. `ingest_status` shows the label counts, the changes waiting and the last full sync.

The commit history adds a `Commit` vertex per commit, keyed by its full SHA, with `MODIFIES` to its files, `DISCUSSED_IN` to its threads, `BACKPATCH_OF` from each other commit of a back-patch group to its first, and `COMMITTED` from the mail author whose name, ignoring case, is the commit's author name. Mail's citations of a commit join its vertex once `resolve_anchors` resolves them to the full SHA. The history's file versions give the `File` and `Subsystem` vertices and the `PART_OF` edges. Its tables have no triggers: its writers record their own changes.

Bootstrap creates each edge label with its indexes. Startup refuses a database whose graph indexes are missing or invalid, rather than build them on full tables: prepare them before upgrading ([graph index preparation](../deploy/README.md#graph-index-preparation)).

## Extraction

`next_extraction_batch` leases the threads that extraction never read, or whose mail changed since, from two lanes: **new mail**, whose latest message is from the last 7 days, oldest first; and the **backfill**, from the newest back to the archive's start. While both have threads, new mail gets at most half the leases. A thread whose mail changes during its lease is leased again. With `PGHACKER_EXTRACTION_ORDER=largest`, the largest threads never extracted go first instead. `ingest_status` shows both lanes (`extraction.lanes`).

Extracting a thread again replaces its participants' positions there, except those a merge moved, which `undo_merge` needs. A feature's release comes from its thread with the latest message, saved by the name it shipped as. Four releases were renamed during their development, and their threads use the development names: 6.6 for 7.0, 7.5 for 8.0, 8.5 for 9.0 and 9.7 for 10 (`shipped_release()`, migration `0021`).

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
| `skills/` | The skills pghacker-mcp serves |
| `home/` | The public home page: React, Vite and TypeScript |
| `scripts/` | The launcher, the site's routes and its edge, crawl and evaluation drivers; the container's supervisor, the home page's server, probe and corpus restore |
| `Dockerfile`, `deploy/` | The image, and the Azure deployment |
| `vendor/tokio-postgres` | A fork of tokio-postgres 0.7.18, which adds `bind_text` |
